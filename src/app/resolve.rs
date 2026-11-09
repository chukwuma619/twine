use crate::engine::{Engine, RECORD_ALREADY, WINDOW_ENDED, cant_do, release_refusal};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{
    Actor, Envelope, Phase, ResolvePayload, Winner, apply_resolve, parse_winner, unix_now,
};
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
            "winner must be buyer, seller, or fiat-owed",
        )]);
    };
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    let actor = engine.trade_actor(&trade, sender);
    let phase = trade.phase()?;
    let window_closed = trade
        .route_by()
        .is_some_and(|deadline| unix_now() >= deadline);
    if actor == Actor::Solver && trade.outcome.is_some() {
        return Ok(vec![cant_do(sender, Some(trade_id), RECORD_ALREADY)]);
    }
    // The missed window is already `unresolved`. A payout must not start in the gap
    // before Fiber's routing deadline. A decision after that deadline still records
    // an outcome when none has been published.
    if actor == Actor::Solver && !window_closed && release_refusal(&trade, unix_now()).is_some() {
        return Ok(vec![cant_do(sender, Some(trade_id), WINDOW_ENDED)]);
    }
    if matches!(phase, Phase::Refunding | Phase::Expired)
        || (phase == Phase::Disputed && window_closed)
    {
        if actor != Actor::Solver {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "only the solver can resolve a dispute",
            )]);
        }
        return match winner {
            Winner::Seller => engine.record_buyer_unpaid(&trade).await,
            Winner::Buyer | Winner::FiatOwed => engine.record_fiat_owed(&trade, sender).await,
        };
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
    match apply_resolve(phase, actor, winner, has_invoice, trade.chat_key_shared) {
        crate::Decision::Ok(Phase::Refunding) => match winner {
            Winner::Seller => engine.record_buyer_unpaid(&trade).await,
            Winner::Buyer | Winner::FiatOwed => engine.record_fiat_owed(&trade, sender).await,
        },
        crate::Decision::Ok(Phase::Releasing) => {
            if let Some(invoice) = &supplied {
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
            engine.db.begin_release(&trade.id)?;
            engine.watch(&trade.id)?;
            Ok(Vec::new())
        }
        crate::Decision::Ok(_) => Ok(Vec::new()),
        crate::Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
        crate::Decision::NoOp => Ok(Vec::new()),
    }
}
