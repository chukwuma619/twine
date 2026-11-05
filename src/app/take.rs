use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::{Outbound, Trade};
use crate::{Envelope, Phase, Side, TakePayload, apply_take, unix_now};
use anyhow::Result;
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
    if side != Side::Sell {
        return Ok(vec![cant_do(sender, None, "only a sell post can be taken")]);
    }
    match apply_take(Phase::Pending) {
        crate::Decision::Ok(Phase::WaitingHold) => {}
        crate::Decision::Reject(reason) => return Ok(vec![cant_do(sender, None, &reason)]),
        _ => return Ok(vec![cant_do(sender, None, "cannot take this order")]),
    }
    let shannons = match validate_take_amount(&payload, &order) {
        Ok(value) => value,
        Err(reason) => return Ok(vec![cant_do(sender, None, &reason)]),
    };
    let invoice = payload
        .invoice
        .as_deref()
        .map(str::trim)
        .filter(|invoice| !invoice.is_empty())
        .map(ToOwned::to_owned);
    let Some(invoice) = invoice else {
        return Ok(vec![cant_do(sender, None, "payout invoice is required")]);
    };
    let trade_id = Uuid::new_v4().to_string();
    let (seller_nostr, buyer_nostr) = (order.maker_nostr.clone(), sender.to_string());
    let created_at = unix_now();
    let trade = Trade {
        id: trade_id.clone(),
        order_id: order.id.clone(),
        seller_nostr,
        seller_fiber: String::new(),
        buyer_nostr,
        buyer_fiber: String::new(),
        fiat_amount: payload.fiat_amount,
        shannons,
        state: Phase::WaitingInvoice.as_str().into(),
        hold_payment_hash: None,
        hold_invoice: None,
        payout_invoice: None,
        payout_payment_hash: None,
        hold_received_at: None,
        hold_created_at: Some(created_at),
        buyer_fiber_addr: None,
    };
    if let Err(error) = engine.db.commit_take(&trade) {
        return Ok(vec![cant_do(sender, None, &error.to_string())]);
    }
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
