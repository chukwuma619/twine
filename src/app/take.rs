use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::{Outbound, Trade};
use crate::{
    Envelope, PAY_INVOICE, PayInvoicePayload, Phase, Side, TakePayload, apply_take, hex_bytes,
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
    if sender == order.maker_nostr {
        return Ok(vec![cant_do(
            sender,
            None,
            "you cannot take your own order",
        )]);
    }
    let Some(side) = Side::parse(&order.side) else {
        return Ok(vec![cant_do(sender, None, "order side is invalid")]);
    };
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
    let Some(method) = order
        .payment_methods
        .iter()
        .find(|method| method.id == payload.payment_method_id)
    else {
        return Ok(vec![cant_do(sender, None, "unknown payment method")]);
    };
    if method.currency != order.fiat_currency.trim() {
        return Ok(vec![cant_do(
            sender,
            None,
            "payment method does not match this currency",
        )]);
    }
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let hash = Sha256::digest(secret);
    let preimage = hex_bytes(&secret);
    let payment_hash = hex_bytes(hash.as_slice());
    let trade_id = Uuid::new_v4().to_string();

    // A sell post offers CKB, so the poster locks. A buy post bids for CKB, so the taker locks.
    let (seller_nostr, seller_fiber, buyer_nostr, buyer_fiber) = match side {
        Side::Sell => (
            order.maker_nostr.clone(),
            order.maker_fiber.clone(),
            sender.to_string(),
            payload.fiber_pubkey,
        ),
        Side::Buy => (
            sender.to_string(),
            payload.fiber_pubkey,
            order.maker_nostr.clone(),
            order.maker_fiber.clone(),
        ),
    };
    let trade = Trade {
        id: trade_id.clone(),
        order_id: order.id.clone(),
        seller_nostr,
        seller_fiber,
        buyer_nostr,
        buyer_fiber,
        fiat_amount: payload.fiat_amount,
        shannons,
        state: Phase::WaitingHold.as_str().into(),
        hold_payment_hash: Some(payment_hash.clone()),
        hold_invoice: None,
        payout_invoice: None,
        payout_payment_hash: None,
        hold_received_at: None,
        payment_method_id: method.id.clone(),
        payment_kind: method.kind.clone(),
        payment_label: method.label.clone(),
        payment_currency: method.currency.clone(),
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
            crate::INVOICE_EXPIRY_SECS,
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
            seller_nostr: trade.seller_nostr.clone(),
            buyer_nostr: trade.buyer_nostr.clone(),
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
