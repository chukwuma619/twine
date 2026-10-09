use crate::constant::{MAX_PAYMENT_METHODS, SHANNONS_PER_CKB};
use crate::{PaymentKind, PaymentMethod, PaymentMethodInput, SupportedPaymentMethod};
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

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
    fiber_pubkey: &str,
) -> Result<u128, AmountError> {
    if fiat_currency.trim().is_empty() {
        return Err(AmountError::Invalid("fiat_currency"));
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

const PAYMENT_TEXT_MAX: usize = 64;

pub fn payment_methods_for_order(
    fiat_currency: &str,
    inputs: &[PaymentMethodInput],
    catalog: &[SupportedPaymentMethod],
) -> Result<Vec<PaymentMethod>, AmountError> {
    if inputs.is_empty() || inputs.len() > MAX_PAYMENT_METHODS {
        return Err(AmountError::Invalid("payment_methods"));
    }
    let currency = fiat_currency.trim();
    let mut seen = HashSet::new();
    let mut methods = Vec::with_capacity(inputs.len());
    for input in inputs {
        let method_id = payment_text(&input.method_id, "payment_method")?;
        if !seen.insert(method_id.clone()) {
            return Err(AmountError::Invalid("payment_methods"));
        }
        let Some(supported) = catalog.iter().find(|method| method.id == method_id) else {
            return Err(AmountError::Invalid("payment_method"));
        };
        if supported.currency != currency {
            return Err(AmountError::Invalid("payment_method"));
        }
        let kind = PaymentKind::parse(&supported.kind).ok_or(AmountError::Invalid("kind"))?;
        methods.push(PaymentMethod {
            id: supported.id.clone(),
            kind: kind.as_str().to_string(),
            label: supported.label.clone(),
            currency: supported.currency.clone(),
        });
    }
    Ok(methods)
}

fn payment_text(value: &str, field: &'static str) -> Result<String, AmountError> {
    let value = value.trim();
    if value.is_empty() || value.len() > PAYMENT_TEXT_MAX {
        return Err(AmountError::Invalid(field));
    }
    Ok(value.to_string())
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
    use crate::{PaymentMethodInput, SupportedPaymentMethod};

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
        assert!(validate_new_order("10", "1500", "1000", "9000", "NGN", "pk").is_ok());
        assert!(validate_new_order("0", "1500", "1000", "9000", "NGN", "pk").is_err());
        assert!(validate_new_order("10", "1500", "9000", "1000", "NGN", "pk").is_err());
    }

    fn catalog() -> Vec<SupportedPaymentMethod> {
        vec![
            SupportedPaymentMethod {
                id: "gtbank".into(),
                kind: "bank".into(),
                label: "GTBank".into(),
                currency: "NGN".into(),
            },
            SupportedPaymentMethod {
                id: "zelle".into(),
                kind: "wallet".into(),
                label: "Zelle".into(),
                currency: "USD".into(),
            },
        ]
    }

    #[test]
    fn a_post_accepts_only_methods_in_its_currency() {
        let catalog = catalog();
        let bank = PaymentMethodInput::new("gtbank");
        let sell = payment_methods_for_order("NGN", &[bank], &catalog).unwrap();
        assert_eq!(sell[0].label, "GTBank");
        assert_eq!(sell[0].currency, "NGN");
        let wallet = PaymentMethodInput::new("zelle");
        let buy = payment_methods_for_order("USD", &[wallet], &catalog).unwrap();
        assert_eq!(buy[0].kind, "wallet");
        assert_eq!(buy[0].currency, "USD");
        let wrong_currency = PaymentMethodInput::new("zelle");
        assert!(payment_methods_for_order("NGN", &[wrong_currency], &catalog).is_err());
        let unknown = PaymentMethodInput::new("paypal");
        assert!(payment_methods_for_order("USD", &[unknown], &catalog).is_err());
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
        PAY_INVOICE, PayInvoicePayload, PaymentMethodInput, TAKE, TakePayload,
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

    pub async fn open_order(
        engine: &Engine<HttpFiber>,
        seller: &str,
        fiber_pubkey: &str,
    ) -> String {
        let outbound = engine
            .handle(
                seller,
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "sell".into(),
                        fiber_pubkey: fiber_pubkey.into(),
                        available_ckb: "10".into(),
                        fiat_currency: "NGN".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                        payment_methods: vec![PaymentMethodInput::new("gtbank")],
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
        buyer: &str,
        order_id: &str,
        fiber_pubkey: &str,
    ) -> (String, String, String) {
        let method_id = engine
            .db
            .get_order(order_id)
            .unwrap()
            .unwrap()
            .payment_methods[0]
            .id
            .clone();
        let payout = engine
            .fiber
            .new_payout_invoice(100_000_000, "twine payout", 36 * 3_600 * 1_000)
            .await
            .unwrap();
        let outbound = engine
            .handle(
                buyer,
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id: order_id.into(),
                        fiat_amount: "1000".into(),
                        fiber_pubkey: fiber_pubkey.into(),
                        payment_method_id: method_id,
                        invoice: Some(payout.invoice),
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

    pub async fn fiat_sent(engine: &Engine<HttpFiber>, buyer: &str, trade_id: &str, invoice: &str) {
        engine
            .handle(
                buyer,
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
