use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Actor, DISPUTED, DisputePayload, Envelope, apply_dispute};
use anyhow::Result;

pub(crate) async fn on_dispute<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let Some(trade_id) = envelope.trade_id.clone() else {
        return Ok(vec![cant_do(sender, None, "trade_id is required")]);
    };
    if engine.solver.is_none() {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id),
            "solver is not configured",
        )]);
    }
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    let order = engine.require_order(&trade.order_id)?;
    if let Some(outbound) = engine.refund_due_outbound(&order, &trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&trade, sender);
    match apply_dispute(trade.phase()?, actor) {
        crate::Decision::Ok(phase) => {
            let payload: DisputePayload = envelope.decode_payload().unwrap_or_default();
            if actor == Actor::Buyer
                && let Some(invoice) = payload
                    .invoice
                    .as_deref()
                    .map(str::trim)
                    .filter(|invoice| !invoice.is_empty())
            {
                engine.db.set_payout(&trade.id, invoice, None)?;
            }
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            engine.watch(&trade.id)?;
            Ok(engine.with_solver(&trade, Envelope::new(DISPUTED).with_trade(&trade.id)))
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
