use crate::constant::{MAX_HOLD_HOURS, MIN_HOLD_HOURS, SHANNONS_PER_CKB};
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AmountError {
    #[error("{0}")]
    Invalid(&'static str),
}

pub fn parse_decimal(value: &str, field: &'static str) -> Result<Decimal, AmountError> {
    let parsed = value
        .trim()
        .parse::<Decimal>()
        .map_err(|_| AmountError::Invalid(field))?;
    if parsed.is_sign_negative() {
        return Err(AmountError::Invalid(field));
    }
    Ok(parsed)
}

pub fn ckb_to_shannons(ckb: Decimal) -> Result<u128, AmountError> {
    let shannons = (ckb * Decimal::from(SHANNONS_PER_CKB))
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
    shannons
        .to_u128()
        .ok_or(AmountError::Invalid("available_ckb"))
}

pub fn shannons_to_ckb(shannons: u128) -> Decimal {
    Decimal::from(shannons) / Decimal::from(SHANNONS_PER_CKB)
}

pub fn shannons_to_ckb_string(shannons: u128) -> String {
    shannons_to_ckb(shannons).normalize().to_string()
}

pub fn fiat_to_shannons(fiat: Decimal, price_per_ckb: Decimal) -> Result<u128, AmountError> {
    if price_per_ckb.is_zero() {
        return Err(AmountError::Invalid("price_per_ckb"));
    }
    let ckb = fiat / price_per_ckb;
    ckb_to_shannons(ckb)
}

pub fn validate_new_order(
    available_ckb: &str,
    price_per_ckb: &str,
    min: &str,
    max: &str,
    fiat_currency: &str,
    payment_method: &str,
    fiber_pubkey: &str,
) -> Result<u128, AmountError> {
    if fiat_currency.trim().is_empty() {
        return Err(AmountError::Invalid("fiat_currency"));
    }
    if payment_method.trim().is_empty() {
        return Err(AmountError::Invalid("payment_method"));
    }
    if fiber_pubkey.trim().is_empty() {
        return Err(AmountError::Invalid("fiber_pubkey"));
    }
    let available = parse_decimal(available_ckb, "available_ckb")?;
    if available.is_zero() {
        return Err(AmountError::Invalid("available_ckb"));
    }
    let price = parse_decimal(price_per_ckb, "price_per_ckb")?;
    if price.is_zero() {
        return Err(AmountError::Invalid("price_per_ckb"));
    }
    let min = parse_decimal(min, "min")?;
    let max = parse_decimal(max, "max")?;
    if min.is_zero() || max < min {
        return Err(AmountError::Invalid("min"));
    }
    let shannons = ckb_to_shannons(available)?;
    if shannons == 0 {
        return Err(AmountError::Invalid("available_ckb"));
    }
    Ok(shannons)
}

pub fn validate_hold_hours(hours: u64) -> Result<u64, AmountError> {
    if !(MIN_HOLD_HOURS..=MAX_HOLD_HOURS).contains(&hours) {
        return Err(AmountError::Invalid("hold_hours"));
    }
    Ok(hours * 3_600)
}

pub fn validate_take(
    fiat_amount: &str,
    min: &str,
    max: &str,
    price_per_ckb: &str,
    available_shannons: u128,
) -> Result<u128, AmountError> {
    let fiat = parse_decimal(fiat_amount, "fiat_amount")?;
    let min = parse_decimal(min, "min")?;
    let max = parse_decimal(max, "max")?;
    let price = parse_decimal(price_per_ckb, "price_per_ckb")?;
    if fiat < min || fiat > max {
        return Err(AmountError::Invalid("fiat_amount"));
    }
    let shannons = fiat_to_shannons(fiat, price)?;
    if shannons == 0 {
        return Err(AmountError::Invalid("fiat_amount"));
    }
    if shannons > available_shannons {
        return Err(AmountError::Invalid("available_ckb"));
    }
    Ok(shannons)
}

pub fn hex_u128(value: u128) -> String {
    format!("0x{value:x}")
}

pub fn hex_u64(value: u64) -> String {
    format!("0x{value:x}")
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

pub fn i64_from_shannons(value: u128) -> Result<i64> {
    i64::try_from(value).context("trade amount does not fit sqlite integer")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_whole_ckb() {
        assert_eq!(ckb_to_shannons(Decimal::from(1)).unwrap(), 100_000_000);
        assert_eq!(shannons_to_ckb_string(250_000_000), "2.5");
    }

    #[test]
    fn rounds_half_up_to_one_shannon() {
        let fiat = Decimal::new(15, 1); // 1.5
        let price = Decimal::from(1);
        assert_eq!(fiat_to_shannons(fiat, price).unwrap(), 150_000_000);
        let odd = Decimal::new(1, 0) / Decimal::from(3);
        let shannons = fiat_to_shannons(odd, Decimal::from(1)).unwrap();
        assert_eq!(shannons, 33_333_333);
    }

    #[test]
    fn take_rejects_out_of_range_and_over_available() {
        assert_eq!(
            validate_take("50", "100", "200", "10", 10_000_000_000).unwrap_err(),
            AmountError::Invalid("fiat_amount")
        );
        assert_eq!(
            validate_take("200", "100", "200", "10", 1).unwrap_err(),
            AmountError::Invalid("available_ckb")
        );
        assert_eq!(
            validate_take("100", "100", "200", "10", 10_000_000_000).unwrap(),
            1_000_000_000
        );
    }

    #[test]
    fn new_order_requires_positive_book() {
        assert!(validate_new_order("10", "1500", "1000", "9000", "NGN", "bank", "pk").is_ok());
        assert!(validate_new_order("0", "1500", "1000", "9000", "NGN", "bank", "pk").is_err());
        assert!(validate_new_order("10", "1500", "9000", "1000", "NGN", "bank", "pk").is_err());
    }

    #[test]
    fn hold_hours_stay_inside_fibers_range() {
        assert_eq!(validate_hold_hours(16).unwrap(), 16 * 3_600);
        assert_eq!(validate_hold_hours(48).unwrap(), 48 * 3_600);
        assert!(validate_hold_hours(15).is_err());
        assert!(validate_hold_hours(49).is_err());
    }
}

/// Live Fiber helpers for engine tests. A node must be running at `TWINE_RPC`.
#[cfg(test)]
pub(crate) mod support {
    use crate::db::Db;
    use crate::engine::Engine;
    use crate::env::invoice_currency;
    use crate::fiber::{HttpFiber, InvoiceInfo, InvoiceRpc, NodeRpc};
    use crate::{
        CANT_DO, Envelope, FIAT_SENT, FiatSentPayload, NEW_ORDER, NewOrderPayload, Outbound,
        PAY_INVOICE, PayInvoicePayload, TAKE, TakePayload,
    };
    use std::env;
    use std::sync::Arc;
    use std::time::Duration;

    pub const DEFAULT_RPC: &str = "http://127.0.0.1:8227";

    pub struct FiberNode {
        pub rpc: Arc<HttpFiber>,
    }

    impl FiberNode {
        pub fn from_env() -> Self {
            let url = env::var("TWINE_RPC").unwrap_or_else(|_| DEFAULT_RPC.to_string());
            let network = env::var("FIBER_NETWORK").unwrap_or_else(|_| "testnet".to_string());
            let currency = invoice_currency(&network).expect("FIBER_NETWORK");
            let token = env::var("TWINE_RPC_TOKEN").unwrap_or_else(|_| "test-token".to_string());
            Self {
                rpc: Arc::new(HttpFiber::new(url, currency, token).expect("TWINE_RPC_TOKEN")),
            }
        }

        pub async fn require_ready(&self) {
            self.rpc
                .node_pubkey()
                .await
                .expect("Fiber RPC is not reachable; start fnn and set TWINE_RPC");
        }
    }

    pub struct TestContext {
        pub engine: Engine<HttpFiber>,
        pub fiber: Arc<HttpFiber>,
    }

    impl TestContext {
        pub async fn new() -> Self {
            let node = FiberNode::from_env();
            node.require_ready().await;
            let db = Arc::new(Db::open_in_memory().expect("sqlite"));
            Self {
                engine: Engine::new(db, node.rpc.clone(), None),
                fiber: node.rpc,
            }
        }
    }

    pub async fn open_order(engine: &Engine<HttpFiber>, maker: &str, fiber_pubkey: &str) -> String {
        let outbound = engine
            .handle(
                maker,
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "sell".into(),
                        fiber_pubkey: fiber_pubkey.into(),
                        available_ckb: "10".into(),
                        fiat_currency: "NGN".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                        payment_method: "bank".into(),
                        hold_hours: 36,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        match &outbound[0] {
            Outbound::PublicOrder(order) => order.order_id.clone(),
            _ => panic!("expected public order"),
        }
    }

    pub async fn take(
        engine: &Engine<HttpFiber>,
        taker: &str,
        order_id: &str,
        taker_fiber_pubkey: &str,
    ) -> (String, String, String) {
        let outbound = engine
            .handle(
                taker,
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id: order_id.into(),
                        fiat_amount: "1000".into(),
                        taker_fiber_pubkey: taker_fiber_pubkey.into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let pay = outbound
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } if envelope.action == PAY_INVOICE => {
                    Some(envelope)
                }
                _ => None,
            })
            .expect("pay-invoice reply");
        let payload: PayInvoicePayload = pay.decode_payload().unwrap();
        (
            pay.trade_id.clone().unwrap(),
            payload.invoice,
            payload.amount_shannons,
        )
    }

    pub async fn fiat_sent(engine: &Engine<HttpFiber>, taker: &str, trade_id: &str, invoice: &str) {
        engine
            .handle(
                taker,
                Envelope::new(FIAT_SENT)
                    .with_trade(trade_id)
                    .with_payload(FiatSentPayload {
                        invoice: invoice.into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    pub fn replies_contain(out: &[Outbound], action: &str) -> bool {
        out.iter().any(|item| match item {
            Outbound::Reply { envelope, .. } => envelope.action == action,
            _ => false,
        })
    }

    pub fn outbound_replies(out: &[Outbound]) -> Vec<Envelope> {
        out.iter()
            .filter_map(|item| match item {
                Outbound::Reply { envelope, .. } => Some(envelope.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn cant_do_reason(out: &[Outbound]) -> String {
        out.iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } if envelope.action == CANT_DO => {
                    let payload: crate::CantDoPayload = envelope.decode_payload().ok()?;
                    Some(payload.reason)
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    pub async fn wait_invoice_status(
        fiber: &HttpFiber,
        payment_hash: &str,
        wanted: &str,
        timeout: Duration,
    ) -> InvoiceInfo {
        let start = tokio::time::Instant::now();
        loop {
            let info = fiber.get_invoice(payment_hash).await.expect("get_invoice");
            if info.status == wanted {
                return info;
            }
            if start.elapsed() > timeout {
                panic!(
                    "invoice {payment_hash} stayed {} after {timeout:?}",
                    info.status
                );
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
}
