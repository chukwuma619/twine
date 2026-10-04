use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::{Outbound, Trade};
use crate::{
    Envelope, PAY_INVOICE, PayInvoicePayload, Phase, TakePayload, apply_take, hex_bytes,
    validate_take,
};
use anyhow::{Result, anyhow};
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) async fn on_take<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let payload: TakePayload = match envelope.decode_payload() {
        Ok(payload) => payload,
        Err(_) => return Ok(vec![cant_do(sender, None, "invalid take payload")]),
    };
    let Some(order) = engine.db.get_order(&payload.order_id)? else {
        return Ok(vec![cant_do(sender, None, "order not found")]);
    };
    if !order.is_open() {
        return Ok(vec![cant_do(sender, None, "order is not open")]);
    }
    if sender == order.seller_nostr {
        return Ok(vec![cant_do(
            sender,
            None,
            "you cannot take your own order",
        )]);
    }
    let has_open = engine.db.open_trade_for_order(&order.id)?.is_some();
    match apply_take(Phase::Pending, has_open) {
        crate::Decision::Ok(Phase::WaitingHold) => {}
        crate::Decision::Reject(reason) => return Ok(vec![cant_do(sender, None, reason)]),
        _ => return Ok(vec![cant_do(sender, None, "cannot take this order")]),
    }
    let shannons = match validate_take(
        &payload.fiat_amount,
        &order.min,
        &order.max,
        &order.price_per_ckb,
        order.available_shannons,
    ) {
        Ok(value) => value,
        Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
    };
    if payload.fiber_pubkey.trim().is_empty() {
        return Ok(vec![cant_do(sender, None, "fiber pubkey is required")]);
    }
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let hash = Sha256::digest(secret);
    let preimage = hex_bytes(&secret);
    let payment_hash = hex_bytes(hash.as_slice());
    let trade_id = Uuid::new_v4().to_string();

    let trade = Trade {
        id: trade_id.clone(),
        order_id: order.id.clone(),
        seller_nostr: order.seller_nostr.clone(),
        seller_fiber: order.seller_fiber.clone(),
        buyer_nostr: sender.to_string(),
        buyer_fiber: payload.fiber_pubkey,
        fiat_amount: payload.fiat_amount,
        shannons,
        state: Phase::WaitingHold.as_str().into(),
        hold_payment_hash: Some(payment_hash.clone()),
        hold_invoice: None,
        payout_invoice: None,
        payout_payment_hash: None,
        hold_received_at: None,
    };
    if let Err(error) = engine.db.commit_take(&trade, &preimage) {
        return Ok(vec![cant_do(sender, None, &error.to_string())]);
    }

    let created = match engine
        .fiber
        .new_hold_invoice(
            shannons,
            &payment_hash,
            &format!("twine {trade_id}"),
            order.hold_secs * 1_000,
        )
        .await
    {
        Ok(created) => created,
        Err(error) => {
            engine
                .db
                .rollback_take(&trade.order_id, &trade.id, &payment_hash, trade.shannons)?;
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                &format!("hold invoice failed: {error}"),
            )]);
        }
    };
    if let Err(error) = engine.fiber.create_preimage(&payment_hash, &preimage).await {
        let _ = engine.fiber.cancel_invoice(&payment_hash).await;
        let _ = engine.fiber.remove_preimage(&payment_hash).await;
        engine
            .db
            .rollback_take(&trade.order_id, &trade.id, &payment_hash, trade.shannons)?;
        return Ok(vec![cant_do(
            sender,
            Some(trade_id),
            &format!("watchtower preimage failed: {error}"),
        )]);
    }
    engine
        .db
        .set_hold(&trade_id, &payment_hash, &created.invoice)?;
    engine.watch(&trade_id)?;

    let order = engine
        .db
        .get_order(&order.id)?
        .ok_or_else(|| anyhow!("order missing after take"))?;
    let pay = Envelope::new(PAY_INVOICE)
        .with_trade(&trade_id)
        .with_payload(PayInvoicePayload {
            invoice: created.invoice,
            amount_shannons: shannons.to_string(),
            order_id: order.id.clone(),
        })?;
    Ok(vec![
        Outbound::PublicOrder(order.public()),
        Outbound::Reply {
            to: trade.seller_nostr.clone(),
            envelope: pay.clone(),
        },
        Outbound::Reply {
            to: trade.buyer_nostr,
            envelope: pay,
        },
    ])
}
