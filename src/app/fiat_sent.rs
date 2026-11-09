use crate::engine::{Engine, cant_do, party_replies, release_refusal};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{
    Envelope, FIAT_SENT_OK, FiatSentOkPayload, FiatSentPayload, Phase, SOLVER_WINDOW_SECS,
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
    let parsed_replacement = if !replacement.is_empty() {
        match engine.require_same_hold_hash(&trade, replacement).await? {
            Ok(parsed) => Some(parsed),
            Err(reason) => return Ok(vec![cant_do(sender, Some(trade_id), &reason)]),
        }
    } else {
        None
    };
    if let Some(outbound) = engine.refund_due_outbound(&trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&trade, sender);
    if !trade.fiat_acknowledged
        && trade.fiat_sent_at.is_some()
        && let Some(reason) = release_refusal(&trade, unix_now())
    {
        return Ok(vec![cant_do(sender, Some(trade_id), reason)]);
    }
    let trade = if let Some(parsed) = parsed_replacement {
        engine.db.set_payout_with_final_delta(
            &trade.id,
            replacement,
            None,
            parsed.final_tlc_expiry_delta_ms,
        )?;
        let Some(trade) = engine.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        trade
    } else {
        trade
    };
    match apply_fiat_sent(trade.phase()?, actor) {
        crate::Decision::Ok(phase) => {
            if phase == Phase::Releasing && trade.fiat_acknowledged {
                engine.db.set_trade_state(&trade.id, phase.as_str())?;
                engine.watch(&trade.id)?;
                return Ok(Vec::new());
            }
            // Route recovery before seller/solver release must not start payout.
            let phase = if phase == Phase::Releasing {
                Phase::FiatSent
            } else {
                phase
            };
            if phase == Phase::FiatSent || phase == Phase::Disputed {
                let first_report = trade.fiat_sent_at.is_none();
                let now = trade.fiat_sent_at.unwrap_or_else(unix_now);
                if phase == Phase::FiatSent {
                    engine.db.mark_fiat_sent(&trade.id, now)?;
                } else if first_report {
                    engine.db.set_fiat_sent_at(&trade.id, now)?;
                }
                engine.watch(&trade.id)?;
                let mut outbound = party_replies(
                    &trade,
                    Envelope::new(FIAT_SENT_OK)
                        .with_trade(&trade.id)
                        .with_payload(FiatSentOkPayload {
                            release_by: now.saturating_add(crate::RELEASE_GRACE_SECS),
                            solver_window_secs: SOLVER_WINDOW_SECS,
                        })?,
                );
                if phase == Phase::FiatSent {
                    let Some(fresh) = engine.db.get_trade(&trade_id)? else {
                        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
                    };
                    if let Some(mut route) = engine.route_after_fiat_sent(&fresh).await? {
                        if !first_report {
                            outbound.clear();
                        }
                        outbound.append(&mut route);
                    }
                }
                return Ok(outbound);
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
