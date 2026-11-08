use crate::engine::{Engine, cant_do, party_replies};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{
    Actor, Envelope, FIAT_SENT_OK, FiatSentOkPayload, FiatSentPayload, Phase, SOLVER_WINDOW_SECS,
    apply_fiat_sent, unix_now,
};
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
    if let Some(outbound) = engine.refund_due_outbound(&trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&trade, sender);
    // The route is checked again only on this click. Waiting-fiat stays put until then.
    if trade.phase()? == Phase::WaitingFiat && actor == Actor::Buyer {
        if !replacement.is_empty() {
            engine.db.set_payout(&trade.id, replacement, None)?;
        }
        let Some(fresh) = engine.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        if let Some(outbound) = engine.refuse_fiat_if_route_blocked(&fresh, sender).await? {
            return Ok(outbound);
        }
    }
    match apply_fiat_sent(trade.phase()?, actor) {
        crate::Decision::Ok(phase) => {
            if !replacement.is_empty() {
                engine.db.set_payout(&trade.id, replacement, None)?;
            }
            if phase == Phase::Releasing {
                engine.db.set_trade_state(&trade.id, phase.as_str())?;
                engine.watch(&trade.id)?;
                return Ok(Vec::new());
            }
            if phase == Phase::FiatSent || phase == Phase::Disputed {
                let now = trade.fiat_sent_at.unwrap_or_else(unix_now);
                if phase == Phase::FiatSent {
                    engine.db.mark_fiat_sent(&trade.id, now)?;
                } else if trade.fiat_sent_at.is_none() {
                    engine.db.set_fiat_sent_at(&trade.id, now)?;
                }
                engine.watch(&trade.id)?;
                return Ok(party_replies(
                    &trade,
                    Envelope::new(FIAT_SENT_OK)
                        .with_trade(&trade.id)
                        .with_payload(FiatSentOkPayload {
                            release_by: now.saturating_add(crate::RELEASE_GRACE_SECS),
                            solver_window_secs: SOLVER_WINDOW_SECS,
                        })?,
                ));
            }
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            engine.watch(&trade.id)?;
            Ok(party_replies(
                &trade,
                Envelope::new(FIAT_SENT_OK).with_trade(&trade.id),
            ))
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
