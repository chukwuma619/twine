use crate::engine::{Engine, cant_do, party_replies};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Actor, DISPUTED, DisputePayload, DisputedPayload, Envelope, apply_dispute};
use anyhow::Result;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

const MAX_EVIDENCE: usize = 32;
const MAX_BLOB_CHARS: usize = 100_000;
const MIN_NIP44: usize = 1 + 32 + 34 + 32;
const MAX_NIP44: usize = 80_000;

const SEND_CHAT: &str = "send this trade's chat to the solver. The receipt cannot be read without it, so the solver cannot pay the buyer";
const INVALID_EVIDENCE: &str = "solver evidence has to be NIP-44 messages addressed to the solver";

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
    let evidence = submitted_evidence(&payload.solver_evidence);
    if trade.phase()? == crate::Phase::Disputed {
        return attach_evidence(engine, sender, &trade, evidence).await;
    }
    if let Some(outbound) = engine.refund_due_outbound(&trade).await? {
        return Ok(outbound);
    }
    if matches!(evidence, SubmittedEvidence::Invalid) {
        return Ok(vec![cant_do(sender, Some(trade_id), INVALID_EVIDENCE)]);
    }
    let actor = engine.trade_actor(&trade, sender);
    let prior = trade.phase()?;
    match apply_dispute(prior, actor) {
        crate::Decision::Ok(phase) => {
            if actor == Actor::Buyer
                && let Some(invoice) = payload
                    .invoice
                    .as_deref()
                    .map(str::trim)
                    .filter(|invoice| !invoice.is_empty())
            {
                let parsed = match engine.require_same_hold_hash(&trade, invoice).await? {
                    Ok(parsed) => parsed,
                    Err(reason) => return Ok(vec![cant_do(sender, Some(trade_id), &reason)]),
                };
                engine.db.set_payout_with_final_delta(
                    &trade.id,
                    invoice,
                    None,
                    parsed.final_tlc_expiry_delta_ms,
                )?;
            }
            if matches!(
                prior,
                crate::Phase::WaitingFiat | crate::Phase::FiatSent | crate::Phase::AwaitingInvoice
            ) {
                engine.db.set_fiat_opened(&trade.id)?;
            }
            engine.db.set_trade_state(&trade.id, phase.as_str())?;
            if let SubmittedEvidence::Ready(blobs) = evidence {
                save_disclosure(engine, &trade.id, sender, blobs)?;
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
            outbound.extend(solver_notice(engine, &trade.id)?);
            Ok(outbound)
        }
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}

/// A dispute is already open. A later message can still seal this trade's chat for the solver.
async fn attach_evidence<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    trade: &crate::Trade,
    evidence: SubmittedEvidence,
) -> Result<Vec<Outbound>> {
    let actor = engine.trade_actor(trade, sender);
    if !matches!(actor, Actor::Buyer | Actor::Seller) {
        return Ok(vec![cant_do(
            sender,
            Some(trade.id.clone()),
            "only a party to the trade can dispute",
        )]);
    }
    let blobs = match evidence {
        SubmittedEvidence::Ready(blobs) => blobs,
        SubmittedEvidence::Invalid => {
            return Ok(vec![cant_do(
                sender,
                Some(trade.id.clone()),
                INVALID_EVIDENCE,
            )]);
        }
        SubmittedEvidence::None => {
            return Ok(vec![cant_do(sender, Some(trade.id.clone()), SEND_CHAT)]);
        }
    };
    save_disclosure(engine, &trade.id, sender, blobs)?;
    solver_notice(engine, &trade.id)
}

fn save_disclosure<F>(
    engine: &Engine<F>,
    trade_id: &str,
    sender: &str,
    blobs: Vec<String>,
) -> Result<()> {
    let existing = engine.db.solver_evidence(trade_id)?;
    let json = merge_disclosure(existing.as_deref(), sender, blobs)?;
    engine.db.save_solver_evidence(trade_id, &json)
}

fn solver_notice<F>(engine: &Engine<F>, trade_id: &str) -> Result<Vec<Outbound>> {
    let Some(solver) = engine.solver.as_deref() else {
        return Ok(Vec::new());
    };
    let stored = engine.db.solver_evidence(trade_id)?;
    if let Some(json) = stored.as_deref()
        && let Some(reply) = solver_reply(solver, trade_id, json)?
    {
        return Ok(vec![reply]);
    }
    Ok(vec![Outbound::Reply {
        to: solver.to_string(),
        envelope: Envelope::new(DISPUTED).with_trade(trade_id),
    }])
}

pub(crate) fn pending_evidence_outbounds(
    db: &crate::db::Db,
    solver: &str,
) -> Result<Vec<Outbound>> {
    let mut outbound = Vec::new();
    for (trade_id, json) in db.pending_solver_evidence()? {
        if let Some(reply) = solver_reply(solver, &trade_id, &json)? {
            outbound.push(reply);
        }
    }
    Ok(outbound)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Disclosure {
    from: String,
    evidence: Vec<String>,
}

#[derive(Serialize)]
struct SolverDispute {
    disclosures: Vec<Disclosure>,
}

pub(crate) fn solver_reply(solver: &str, trade_id: &str, json: &str) -> Result<Option<Outbound>> {
    let disclosures: Vec<Disclosure> = serde_json::from_str(json).unwrap_or_default();
    if disclosures.is_empty() {
        return Ok(None);
    }
    Ok(Some(Outbound::Reply {
        to: solver.to_string(),
        envelope: Envelope::new(DISPUTED)
            .with_trade(trade_id)
            .with_payload(SolverDispute { disclosures })?,
    }))
}

pub(crate) fn merge_disclosure(
    existing: Option<&str>,
    from: &str,
    evidence: Vec<String>,
) -> Result<String> {
    let mut rows: Vec<Disclosure> = existing
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    rows.retain(|row| row.from != from);
    rows.push(Disclosure {
        from: from.to_string(),
        evidence,
    });
    Ok(serde_json::to_string(&rows)?)
}

enum SubmittedEvidence {
    None,
    Invalid,
    Ready(Vec<String>),
}

fn submitted_evidence(values: &[String]) -> SubmittedEvidence {
    if values.is_empty() {
        return SubmittedEvidence::None;
    }
    if values.len() > MAX_EVIDENCE {
        return SubmittedEvidence::Invalid;
    }
    let mut ready = Vec::with_capacity(values.len());
    for value in values {
        let Some(blob) = nip44_blob(value) else {
            return SubmittedEvidence::Invalid;
        };
        ready.push(blob);
    }
    SubmittedEvidence::Ready(ready)
}

fn nip44_blob(value: &str) -> Option<String> {
    let blob = value.trim();
    if blob.is_empty()
        || blob.len() > MAX_BLOB_CHARS
        || blob.as_bytes().iter().any(u8::is_ascii_whitespace)
    {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .ok()?;
    if bytes.len() < MIN_NIP44 || bytes.len() > MAX_NIP44 || bytes[0] != 2 {
        return None;
    }
    Some(blob.to_string())
}

#[cfg(test)]
pub(crate) fn sample_solver_evidence() -> String {
    let mut bytes = vec![2_u8];
    bytes.extend(std::iter::repeat_n(7, MIN_NIP44 - 1));
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_key_is_not_evidence() {
        let payload: DisputePayload =
            serde_json::from_str(&format!(r#"{{"conversation_key":"{}"}}"#, "ab".repeat(32)))
                .unwrap();
        assert!(payload.solver_evidence.is_empty());
        assert!(matches!(
            submitted_evidence(&["ab".repeat(32)]),
            SubmittedEvidence::Invalid
        ));
        assert!(matches!(
            submitted_evidence(&[sample_solver_evidence()]),
            SubmittedEvidence::Ready(_)
        ));
    }

    #[test]
    fn both_parties_keep_their_own_disclosure() {
        let buyer = sample_solver_evidence();
        let seller = {
            let mut bytes = vec![2_u8];
            bytes.extend(std::iter::repeat_n(9, MIN_NIP44 - 1));
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let first = merge_disclosure(None, "buyer", vec![buyer.clone()]).unwrap();
        let second = merge_disclosure(Some(&first), "seller", vec![seller.clone()]).unwrap();
        let replaced = {
            let mut bytes = vec![2_u8];
            bytes.extend(std::iter::repeat_n(4, MIN_NIP44 - 1));
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let third = merge_disclosure(Some(&second), "buyer", vec![replaced.clone()]).unwrap();
        let rows: Vec<Disclosure> = serde_json::from_str(&third).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].from, "seller");
        assert_eq!(rows[0].evidence, vec![seller]);
        assert_eq!(rows[1].from, "buyer");
        assert_eq!(rows[1].evidence, vec![replaced]);
    }
}
