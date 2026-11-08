use crate::engine::{Engine, cant_do, party_replies};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Actor, DISPUTED, DisputePayload, DisputedPayload, Envelope, apply_dispute};
use anyhow::Result;
use serde::Serialize;

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
    let payload: DisputePayload = envelope.decode_payload().unwrap_or_default();
    if trade.phase()? == crate::Phase::Disputed {
        return attach_chat_key(engine, sender, &trade, &payload).await;
    }
    if let Some(outbound) = engine.refund_due_outbound(&trade).await? {
        return Ok(outbound);
    }
    let actor = engine.trade_actor(&trade, sender);
    match apply_dispute(trade.phase()?, actor) {
        crate::Decision::Ok(phase) => {
            if actor == Actor::Buyer
                && let Some(invoice) = payload
                    .invoice
                    .as_deref()
                    .map(str::trim)
                    .filter(|invoice| !invoice.is_empty())
            {
                if let Err(reason) = engine.require_same_hold_hash(&trade, invoice).await? {
                    return Ok(vec![cant_do(sender, Some(trade_id), &reason)]);
                }
                engine.db.set_payout(&trade.id, invoice, None)?;
            }
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            if conversation_key(payload.conversation_key.as_deref()).is_some() {
                engine.db.set_chat_key_shared(&trade.id)?;
            }
            engine.watch(&trade.id)?;
            let mut outbound = party_replies(
                &trade,
                Envelope::new(DISPUTED)
                    .with_trade(&trade.id)
                    .with_payload(DisputedPayload {
                        solver: engine.solver.clone(),
                    })?,
            );
            if let Some(solver) = &engine.solver {
                let mut envelope = Envelope::new(DISPUTED).with_trade(&trade.id);
                if let Some(key) = conversation_key(payload.conversation_key.as_deref()) {
                    envelope = envelope.with_payload(SolverDispute {
                        conversation_key: key,
                    })?;
                }
                outbound.push(Outbound::Reply {
                    to: solver.clone(),
                    envelope,
                });
            }
            Ok(outbound)
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}

/// A dispute is already open. A later message can still hand the solver the chat key.
async fn attach_chat_key<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    trade: &crate::Trade,
    payload: &DisputePayload,
) -> Result<Vec<Outbound>> {
    let actor = engine.trade_actor(trade, sender);
    if !matches!(actor, Actor::Buyer | Actor::Seller) {
        return Ok(vec![cant_do(
            sender,
            Some(trade.id.clone()),
            "only a party to the trade can dispute",
        )]);
    }
    let Some(key) = conversation_key(payload.conversation_key.as_deref()) else {
        return Ok(vec![cant_do(
            sender,
            Some(trade.id.clone()),
            "the chat key has to be 32 bytes of hex",
        )]);
    };
    engine.db.set_chat_key_shared(&trade.id)?;
    let Some(solver) = engine.solver.clone() else {
        return Ok(vec![cant_do(
            sender,
            Some(trade.id.clone()),
            "solver is not configured",
        )]);
    };
    Ok(vec![Outbound::Reply {
        to: solver,
        envelope: Envelope::new(DISPUTED)
            .with_trade(&trade.id)
            .with_payload(SolverDispute {
                conversation_key: key,
            })?,
    }])
}

#[derive(Serialize)]
struct SolverDispute {
    conversation_key: String,
}

/// 32-byte hex. Anything else is dropped so a bad key does not block the dispute.
fn conversation_key(value: Option<&str>) -> Option<String> {
    let key = value?.trim().to_ascii_lowercase();
    if key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(key)
    } else {
        None
    }
}
