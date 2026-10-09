use crate::engine::{Engine, cant_do, party_replies};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Envelope, FIAT_SENT_OK, FiatSentPayload, Phase, apply_fiat_sent};
use anyhow::Result;

pub(crate) async fn on_fiat_sent<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let Some(trade_id) = envelope.trade_id.clone() else {
        return Ok(vec![cant_do(sender, None, "trade_id is required")]);
    };
    let payload: FiatSentPayload = match envelope.decode_payload() {
        Ok(payload) => payload,
        Err(_) => {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "invalid fiat-sent payload",
            )]);
        }
    };
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    let replacement = payload.invoice.trim();
    let stored = trade
        .payout_invoice
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty();
    if replacement.is_empty() && stored {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id),
            "payout invoice is required",
        )]);
    }
    if !replacement.is_empty()
        && let Err(reason) = engine.require_same_hold_hash(&trade, replacement).await?
    {
        return Ok(vec![cant_do(sender, Some(trade_id), &reason)]);
    }
    let order = engine.require_order(&trade.order_id)?;
    if let Some(outbound) = engine.refund_due_outbound(&order, &trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&trade, sender);
    match apply_fiat_sent(trade.phase()?, actor) {
        crate::Decision::Ok(phase) => {
            if !replacement.is_empty() {
                engine.db.set_payout(&trade.id, replacement, None)?;
            }
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            engine.watch(&trade.id)?;
            if phase == Phase::Releasing {
                return Ok(Vec::new());
            }
            Ok(party_replies(
                &trade,
                Envelope::new(FIAT_SENT_OK).with_trade(&trade.id),
            ))
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
