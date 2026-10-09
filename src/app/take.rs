use crate::engine::{Engine, cant_do, need_invoice_replies};
use crate::fiber::FiberRpc;
use crate::types::{Outbound, Trade};
use crate::{Envelope, Phase, Side, TakePayload, apply_take, unix_now};
use anyhow::{Result, anyhow};
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
        crate::Decision::Reject(reason) => return Ok(vec![cant_do(sender, None, &reason)]),
        _ => return Ok(vec![cant_do(sender, None, "cannot take this order")]),
    }
    let shannons = match validate_take_amount(&payload, &order) {
        Ok(value) => value,
        Err(reason) => return Ok(vec![cant_do(sender, None, &reason)]),
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
    let invoice = payload
        .invoice
        .as_deref()
        .map(str::trim)
        .filter(|invoice| !invoice.is_empty())
        .map(ToOwned::to_owned);
    // Sell post: the taker is the buyer and the invoice is in this message.
    // Buy post: the maker is the buyer and still has to send the invoice.
    if side == Side::Sell && invoice.is_none() {
        return Ok(vec![cant_do(sender, None, "payout invoice is required")]);
    }
    let trade_id = Uuid::new_v4().to_string();
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
    let created_at = unix_now();
    let trade = Trade {
        id: trade_id.clone(),
        order_id: order.id.clone(),
        seller_nostr,
        seller_fiber,
        buyer_nostr,
        buyer_fiber,
        fiat_amount: payload.fiat_amount,
        shannons,
        state: Phase::WaitingInvoice.as_str().into(),
        hold_payment_hash: None,
        hold_invoice: None,
        payout_invoice: None,
        payout_payment_hash: None,
        hold_received_at: None,
        hold_created_at: Some(created_at),
        payment_method_id: method.id.clone(),
        payment_kind: method.kind.clone(),
        payment_label: method.label.clone(),
        payment_currency: method.currency.clone(),
    };
    if let Err(error) = engine.db.commit_take(&trade) {
        return Ok(vec![cant_do(sender, None, &error.to_string())]);
    }
    let Some(invoice) = invoice else {
        engine.watch(&trade_id)?;
        let order = engine
            .db
            .get_order(&order.id)?
            .ok_or_else(|| anyhow!("order missing after take"))?;
        return need_invoice_replies(&trade, &order);
    };
    match engine.bind_buyer_hold(&trade, &invoice).await? {
        Ok(outbound) => Ok(outbound),
        Err(reason) => {
            engine
                .db
                .rollback_take(&trade.order_id, &trade.id, "", trade.shannons)?;
            Ok(vec![cant_do(sender, Some(trade_id), &reason)])
        }
    }
}

fn validate_take_amount(
    payload: &TakePayload,
    order: &crate::types::Order,
) -> Result<u128, String> {
    crate::validate_take(
        &payload.fiat_amount,
        &order.min,
        &order.max,
        &order.price_per_ckb,
        order.available_shannons,
    )
    .map_err(|error| error.to_string())
}
