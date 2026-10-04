use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Envelope, apply_release};
use anyhow::Result;

pub(crate) async fn on_release<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let Some(trade_id) = envelope.trade_id.clone() else {
        return Ok(vec![cant_do(sender, None, "trade_id is required")]);
    };
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    let order = engine.require_order(&trade.order_id)?;
    if let Some(outbound) = engine.refund_due_outbound(&order, &trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&order, &trade, sender)?;
    let has_invoice = trade
        .payout_invoice
        .as_deref()
        .is_some_and(|invoice| !invoice.trim().is_empty());
    match apply_release(trade.phase()?, actor, has_invoice) {
        crate::Decision::Ok(phase) => {
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            engine.watch(&trade.id)?;
            Ok(Vec::new())
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
