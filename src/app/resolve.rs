use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Envelope, Phase, ResolvePayload, apply_resolve, parse_winner};
use anyhow::Result;

pub(crate) async fn on_resolve<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let Some(trade_id) = envelope.trade_id.clone() else {
        return Ok(vec![cant_do(sender, None, "trade_id is required")]);
    };
    let payload: ResolvePayload = match envelope.decode_payload() {
        Ok(payload) => payload,
        Err(_) => {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "invalid resolve payload",
            )]);
        }
    };
    let Some(winner) = parse_winner(&payload.winner) else {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id),
            "winner must be buyer or seller",
        )]);
    };
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    let order = engine.require_order(&trade.order_id)?;
    if let Some(outbound) = engine.refund_due_outbound(&order, &trade).await? {
        return Ok(outbound);
    }
    let supplied = payload
        .invoice
        .as_deref()
        .map(str::trim)
        .filter(|invoice| !invoice.is_empty())
        .map(ToOwned::to_owned);
    let has_invoice = supplied.is_some()
        || trade
            .payout_invoice
            .as_deref()
            .is_some_and(|invoice| !invoice.trim().is_empty());
    let actor = engine.trade_actor(&order, &trade, sender)?;
    match apply_resolve(trade.phase()?, actor, winner, has_invoice) {
        crate::Decision::Ok(Phase::Refunding) => engine.begin_refund(&order, &trade).await,
        crate::Decision::Ok(Phase::Releasing) => {
            if let Some(invoice) = &supplied {
                engine.db.set_payout(&trade.id, invoice, None)?;
            }
            engine
                .db
                .set_trade_state(&trade.id, Phase::Releasing.as_str())?;
            engine.watch(&trade.id)?;
            Ok(Vec::new())
        }
        crate::Decision::Ok(_) => Ok(Vec::new()),
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
