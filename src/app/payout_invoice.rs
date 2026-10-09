use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::Outbound;
use crate::{Envelope, PayoutInvoicePayload};
use anyhow::Result;

pub(crate) async fn on_payout_invoice<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let Some(trade_id) = envelope.trade_id.clone() else {
        return Ok(vec![cant_do(sender, None, "trade_id is required")]);
    };
    let payload: PayoutInvoicePayload = match envelope.decode_payload() {
        Ok(payload) => payload,
        Err(_) => {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "invalid payout-invoice payload",
            )]);
        }
    };
    let invoice = payload.invoice.trim();
    if invoice.is_empty() {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id.clone()),
            "payout invoice is required",
        )]);
    }
    let Some(trade) = engine.db.get_trade(&trade_id)? else {
        return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
    };
    if sender != trade.buyer_nostr {
        return Ok(vec![cant_do(
            sender,
            Some(trade_id),
            "only the buyer can send the payout invoice",
        )]);
    }
    match engine.bind_buyer_hold(&trade, invoice).await? {
        Ok(outbound) => Ok(outbound),
        Err(reason) => Ok(vec![cant_do(sender, Some(trade_id), &reason)]),
    }
}
