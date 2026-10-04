use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::{Order, Outbound};
use crate::{
    Envelope, NewOrderPayload, OrderStatus, Side, validate_hold_hours, validate_new_order,
};
use anyhow::Result;
use uuid::Uuid;

pub(crate) async fn on_new_order<F: FiberRpc + 'static>(
    engine: &Engine<F>,
    sender: &str,
    envelope: Envelope,
) -> Result<Vec<Outbound>> {
    let payload: NewOrderPayload = match envelope.decode_payload() {
        Ok(payload) => payload,
        Err(_) => return Ok(vec![cant_do(sender, None, "invalid new-order payload")]),
    };
    let available = match validate_new_order(
        &payload.available_ckb,
        &payload.price_per_ckb,
        &payload.min,
        &payload.max,
        &payload.fiat_currency,
        &payload.payment_method,
        &payload.fiber_pubkey,
    ) {
        Ok(value) => value,
        Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
    };
    let Some(side) = Side::parse(payload.side.trim()) else {
        return Ok(vec![cant_do(sender, None, "side must be buy or sell")]);
    };
    let hold_secs = match validate_hold_hours(payload.hold_hours) {
        Ok(value) => value,
        Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
    };
    let order = Order {
        id: Uuid::new_v4().to_string(),
        side: side.as_str().to_string(),
        maker_nostr: sender.to_string(),
        maker_fiber: payload.fiber_pubkey,
        available_shannons: available,
        fiat_currency: payload.fiat_currency,
        price_per_ckb: payload.price_per_ckb,
        min: payload.min,
        max: payload.max,
        payment_method: payload.payment_method,
        status: OrderStatus::Open.as_str().into(),
        hold_secs,
    };
    engine.db.insert_order(&order)?;
    Ok(vec![Outbound::PublicOrder(order.public())])
}
