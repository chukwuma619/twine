use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{CANCELED, CancelPayload, Envelope, InvoiceStatus, Phase, apply_cancel, order_actor};
use anyhow::Result;

pub(crate) async fn on_cancel<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    if let Some(trade_id) = envelope.trade_id.clone() {
        return cancel_trade(engine, sender, &trade_id).await;
    }
    let payload: CancelPayload = envelope
        .decode_payload()
        .unwrap_or(CancelPayload { order_id: None });
    let Some(order_id) = payload.order_id else {
        return Ok(vec![cant_do(
            sender,
            None,
            "order_id or trade_id is required",
        )]);
    };
    let Some(order) = engine.db.get_order(&order_id)? else {
        return Ok(vec![cant_do(sender, None, "order not found")]);
    };
    if engine.db.open_trade_for_order(&order.id)?.is_some() {
        return Ok(vec![cant_do(sender, None, "order has an open trade")]);
    }
    let actor = order_actor(&order.seller_nostr, engine.solver.as_deref(), sender);
    match apply_cancel(Phase::Pending, actor, None) {
        crate::Decision::Ok(Phase::Canceled) => {
            if let Err(error) = engine.db.cancel_order(&order.id) {
                return Ok(vec![cant_do(sender, None, &error.to_string())]);
            }
            let order = engine.require_order(&order.id)?;
            Ok(vec![
                Outbound::PublicOrder(order.public()),
                Outbound::Reply {
                    to: sender.to_string(),
                    envelope: Envelope::new(CANCELED),
                },
            ])
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, None, reason)]),
        _ => Ok(vec![cant_do(sender, None, "cannot cancel order")]),
    }
}

async fn cancel_trade<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    trade_id: &str,
) -> Result<Vec<Outbound>> {
    let Some(trade) = engine.db.get_trade(trade_id)? else {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id.to_string()),
            "trade not found",
        )]);
    };
    let order = engine.require_order(&trade.order_id)?;
    let actor = engine.trade_actor(&trade, sender);
    let invoice_status = if let Some(hash) = trade.hold_payment_hash.as_deref() {
        let invoice = engine.fiber.get_invoice(hash).await?;
        InvoiceStatus::parse(&invoice.status)
    } else {
        Some(InvoiceStatus::Open)
    };
    match apply_cancel(trade.phase()?, actor, invoice_status) {
        crate::Decision::Ok(Phase::Canceled) => {
            if invoice_status == Some(InvoiceStatus::Open) {
                if let Some(hash) = trade.hold_payment_hash.as_deref() {
                    engine.fiber.cancel_invoice(hash).await?;
                }
            }
            engine.complete_cancel(&order, &trade).await
        }
        crate::Decision::Ok(Phase::Expired) => engine.expire_trade(&order, &trade).await,
        crate::Decision::Reject(reason) => {
            Ok(vec![cant_do(sender, Some(trade_id.to_string()), reason)])
        }
        _ => Ok(vec![cant_do(
            sender,
            Some(trade_id.to_string()),
            "cannot cancel",
        )]),
    }
}
