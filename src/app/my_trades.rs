use crate::engine::Engine;
use crate::fiber::FiberRpc;
use crate::types::{Outbound, TradeSnapshot, TradesPayload};
use crate::{Envelope, TRADES};
use anyhow::Result;

pub(crate) fn on_my_trades<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
) -> Result<Vec<Outbound>> {
    let trades = engine.db.trades_for_party(sender)?;
    let mut snapshots = Vec::new();
    for trade in trades {
        let Some(order) = engine.db.get_order(&trade.order_id)? else {
            continue;
        };
        snapshots.push(TradeSnapshot::from_trade(
            &trade,
            &order.fiat_currency,
            order.hold_secs,
        ));
    }
    Ok(vec![Outbound::Reply {
        to: sender.to_string(),
        envelope: Envelope::new(TRADES).with_payload(TradesPayload { trades: snapshots })?,
    }])
}
