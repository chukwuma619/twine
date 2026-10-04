use crate::engine::{Engine, cant_do};
use crate::fiber::FiberRpc;
use crate::types::{Order, Outbound};
use crate::{
    Envelope, NewOrderPayload, OrderStatus, Side, payment_methods_for_order, validate_new_order,
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
    let Some(side) = Side::parse(payload.side.trim()) else {
        return Ok(vec![cant_do(sender, None, "side must be sell or buy")]);
    };
    let available = match validate_new_order(
        &payload.available_ckb,
        &payload.price_per_ckb,
        &payload.min,
        &payload.max,
        &payload.fiat_currency,
        &payload.fiber_pubkey,
    ) {
        Ok(value) => value,
        Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
    };
    let catalog = engine.db.supported_payment_methods()?;
    let payment_methods =
        match payment_methods_for_order(&payload.fiat_currency, &payload.payment_methods, &catalog)
        {
            Ok(methods) => methods,
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
        payment_methods,
        status: OrderStatus::Open.as_str().into(),
        hold_secs: crate::HOLD_SECS,
    };
    engine.db.insert_order(&order)?;
    Ok(vec![Outbound::PublicOrder(order.public())])
}
