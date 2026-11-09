use crate::constant::SHANNONS_PER_CKB;
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn unix_now_ms() -> i64 {
    unix_now() * 1000
}

/// Claim margin from the inbound channel's `tlc_expiry_delta`, floored at Fiber's default.
pub fn claim_margin_ms(channel_tlc_expiry_delta_ms: Option<u64>) -> u64 {
    let delay = channel_tlc_expiry_delta_ms.unwrap_or(crate::FIBER_TLC_EXPIRY_DELAY_MS);
    delay
        .saturating_add(crate::FIBER_CHECK_CHANNELS_MS)
        .max(crate::CLAIM_MARGIN_MS)
}

/// Last second a payout can still start before Fiber drops the incoming TLC.
pub fn payout_deadline_secs(expires_at_ms: i64, claim_margin_ms: u64, final_delta_ms: u64) -> i64 {
    let needed = final_delta_ms.saturating_add(crate::FIBER_RANDOM_EXPIRY_RESERVE_MS) as i64;
    expires_at_ms
        .saturating_sub(claim_margin_ms as i64)
        .saturating_sub(needed)
        / 1000
}

/// Last second the buyer may mark fiat sent.
///
/// Leaves the 2-hour release grace and the 4-hour solver window in front of
/// [payout_deadline_secs]. That routing deadline is at least [crate::ROUTE_BUFFER_SECS]
/// before the hold ends, and longer when Fiber needs more TLC time.
pub fn fiat_deadline_secs(expires_at_ms: i64, claim_margin_ms: u64, final_delta_ms: u64) -> i64 {
    payout_deadline_secs(expires_at_ms, claim_margin_ms, final_delta_ms)
        .saturating_sub(crate::RELEASE_GRACE_SECS)
        .saturating_sub(crate::SOLVER_WINDOW_SECS)
}

/// Seconds a payout invoice must still be payable when the hold is created.
pub fn payout_invoice_must_last_secs(
    hold_secs: u64,
    claim_margin_ms: u64,
    final_delta_ms: u64,
) -> u64 {
    let slack_secs = (claim_margin_ms
        .saturating_add(final_delta_ms)
        .saturating_add(crate::FIBER_RANDOM_EXPIRY_RESERVE_MS))
        / 1000;
    crate::INVOICE_EXPIRY_SECS
        .saturating_add(hold_secs)
        .saturating_sub(slack_secs)
}

/// No expiry stays payable. Fiber still accepts an invoice at the exact expiry instant.
pub fn invoice_payable_through(expires_at_ms: Option<i64>, until_ms: i64) -> bool {
    match expires_at_ms {
        None => true,
        Some(expires_at_ms) => expires_at_ms >= until_ms,
    }
}

/// `tlc_expiry_limit` for this invoice, or a shorter invoice, or a refund.
pub fn payout_budget(
    expires_at_ms: i64,
    now_ms: i64,
    claim_margin_ms: u64,
    final_delta_ms: u64,
) -> PayoutBudget {
    let budget = expires_at_ms
        .saturating_sub(claim_margin_ms as i64)
        .saturating_sub(now_ms);
    let budget = u64::try_from(budget).unwrap_or(0);
    let needed = final_delta_ms.saturating_add(crate::FIBER_RANDOM_EXPIRY_RESERVE_MS);
    let shortest =
        crate::MIN_TLC_EXPIRY_LIMIT_MS.saturating_add(crate::FIBER_RANDOM_EXPIRY_RESERVE_MS);
    if budget >= needed {
        PayoutBudget::Send {
            tlc_expiry_limit_ms: budget.min(crate::MAX_TLC_EXPIRY_LIMIT_MS),
        }
    } else if budget >= shortest {
        PayoutBudget::ShorterInvoice
    } else {
        PayoutBudget::Refund
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutBudget {
    Send { tlc_expiry_limit_ms: u64 },
    ShorterInvoice,
    Refund,
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
) -> Result<u128, AmountError> {
    if fiat_currency.trim().is_empty() {
        return Err(AmountError::Invalid("fiat_currency"));
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
    if min > available * price {
        return Err(AmountError::Invalid("min"));
    }
    let shannons = ckb_to_shannons(available)?;
    if shannons == 0 {
        return Err(AmountError::Invalid("available_ckb"));
    }
    Ok(shannons)
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

/// Fiber `connect_peer` multiaddr. Loopback and LAN addresses are refused.
pub fn parse_fiber_addr(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(trimmed) = value.map(str::trim).filter(|addr| !addr.is_empty()) else {
        return Ok(None);
    };
    if !trimmed.starts_with('/') {
        return Err("fiber listen address must be a multiaddr, like /ip4/1.2.3.4/tcp/8228".into());
    }
    if !fiber_addr_is_reachable(trimmed) {
        return Err(
            "fiber listen address must be reachable from the internet, not 127.0.0.1 or a LAN address. Connect your node to the hub instead".into(),
        );
    }
    Ok(Some(trimmed.to_string()))
}

pub fn fiber_addr_is_reachable(addr: &str) -> bool {
    let parts: Vec<&str> = addr
        .trim()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let Some(protocol) = parts.first().copied() else {
        return false;
    };
    let Some(value) = parts.get(1).copied() else {
        return false;
    };
    match protocol {
        "ip4" | "ip6" => value
            .parse::<std::net::IpAddr>()
            .ok()
            .is_some_and(ip_is_reachable),
        "dns" | "dns4" | "dns6" => {
            let host = value.to_ascii_lowercase();
            host != "localhost" && !host.ends_with(".localhost")
        }
        "onion" | "onion3" => true,
        _ => false,
    }
}

fn ip_is_reachable(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast())
        }
        std::net::IpAddr::V6(ip) => {
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_unicast_link_local()
                || (ip.segments()[0] & 0xfe00) == 0xfc00)
        }
    }
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
    fn fiat_deadline_leaves_the_release_grace_and_solver_window() {
        let expires_at_ms = 50_000_000_000_i64;
        let claim = crate::CLAIM_MARGIN_MS;
        let final_delta = crate::MIN_TLC_EXPIRY_LIMIT_MS;
        let route = payout_deadline_secs(expires_at_ms, claim, final_delta);
        let fiat = fiat_deadline_secs(expires_at_ms, claim, final_delta);
        let hold_end = expires_at_ms / 1000;
        assert_eq!(
            fiat,
            route - crate::RELEASE_GRACE_SECS - crate::SOLVER_WINDOW_SECS
        );
        assert!(route <= hold_end - crate::ROUTE_BUFFER_SECS);
        assert!(
            fiat <= hold_end
                - crate::RELEASE_GRACE_SECS
                - crate::SOLVER_WINDOW_SECS
                - crate::ROUTE_BUFFER_SECS
        );
    }

    #[test]
    fn deadlines_follow_the_invoice_lock_and_channel_delay() {
        let expires_at_ms = 50_000_000_000_i64;
        let short = payout_deadline_secs(
            expires_at_ms,
            crate::CLAIM_MARGIN_MS,
            crate::MIN_TLC_EXPIRY_LIMIT_MS,
        );
        let long = payout_deadline_secs(
            expires_at_ms,
            crate::CLAIM_MARGIN_MS,
            crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS,
        );
        assert!(long < short);
        let slow_channel = claim_margin_ms(Some(8 * 60 * 60 * 1000));
        assert!(slow_channel > crate::CLAIM_MARGIN_MS);
        assert!(
            payout_deadline_secs(expires_at_ms, slow_channel, crate::MIN_TLC_EXPIRY_LIMIT_MS)
                < short
        );
    }

    #[test]
    fn a_payout_invoice_has_to_outlast_the_hold() {
        let must_last = payout_invoice_must_last_secs(
            crate::HOLD_SECS,
            crate::CLAIM_MARGIN_MS,
            crate::MIN_TLC_EXPIRY_LIMIT_MS,
        );
        assert!(must_last > 24 * 3_600);
        assert!(must_last < crate::HOLD_SECS);
        let now = 1_000_000_i64;
        let until = now + i64::try_from(must_last).unwrap() * 1000;
        assert!(invoice_payable_through(None, until));
        assert!(invoice_payable_through(Some(until), until));
        assert!(!invoice_payable_through(Some(until - 1), until));
    }

    #[test]
    fn fiber_addr_is_a_multiaddr() {
        assert_eq!(parse_fiber_addr(None).unwrap(), None);
        assert_eq!(parse_fiber_addr(Some("  ")).unwrap(), None);
        assert_eq!(
            parse_fiber_addr(Some("/ip4/1.2.3.4/tcp/8228")).unwrap(),
            Some("/ip4/1.2.3.4/tcp/8228".into())
        );
        assert!(parse_fiber_addr(Some("1.2.3.4:8228")).is_err());
        assert!(parse_fiber_addr(Some("/ip4/127.0.0.1/tcp/8228")).is_err());
        assert!(parse_fiber_addr(Some("/ip4/192.168.1.10/tcp/8228")).is_err());
        assert!(parse_fiber_addr(Some("/dns4/localhost/tcp/8228")).is_err());
        assert!(fiber_addr_is_reachable(
            "/dns4/bottle.fiber.channel/tcp/8119"
        ));
    }

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
        assert!(validate_new_order("10", "1500", "1000", "9000", "NGN").is_ok());
        assert!(validate_new_order("0", "1500", "1000", "9000", "NGN").is_err());
        assert!(validate_new_order("10", "1500", "9000", "1000", "NGN").is_err());
        assert!(validate_new_order("1", "1500", "2000", "9000", "NGN").is_err());
    }
}

/// Live Fiber helpers for engine tests. Hub at `TWINE_RPC`; settle paths also need seller/buyer RPCs.
#[cfg(test)]
pub(crate) mod support {
    use crate::db::Db;
    use crate::engine::Engine;
    use crate::env::invoice_currency;
    use crate::fiber::{HttpFiber, InvoiceInfo, InvoiceRpc, NodeRpc, PaymentRpc};
    use crate::{
        CANT_DO, Envelope, FIAT_SENT, FiatSentPayload, NEW_ORDER, NewOrderPayload, Outbound,
        PAY_INVOICE, PayInvoicePayload, TAKE, TakePayload,
    };
    use std::env;
    use std::sync::Arc;
    use std::time::Duration;

    pub const DEFAULT_RPC: &str = "http://127.0.0.1:8229";

    pub struct FiberNode {
        pub rpc: Arc<HttpFiber>,
    }

    impl FiberNode {
        pub fn from_env() -> Self {
            let url = env::var("TWINE_RPC").unwrap_or_else(|_| DEFAULT_RPC.to_string());
            Self {
                rpc: Arc::new(http_fiber_from_env(&url, "TWINE_RPC_TOKEN")),
            }
        }

        pub async fn require_ready(&self) {
            self.rpc
                .node_pubkey()
                .await
                .expect("Fiber RPC is not reachable; run make e2e-up");
        }
    }

    fn network_currency() -> &'static str {
        let network = env::var("FIBER_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        invoice_currency(network.trim()).expect("FIBER_NETWORK")
    }

    fn http_fiber_from_env(url: &str, token_var: &str) -> HttpFiber {
        let currency = network_currency();
        let token = env::var(token_var).unwrap_or_else(|_| {
            env::var("TWINE_RPC_TOKEN").unwrap_or_else(|_| "test-token".to_string())
        });
        HttpFiber::new(url, currency, token).expect(token_var)
    }

    fn optional_peer(url_var: &str, token_var: &str) -> Option<Arc<HttpFiber>> {
        let url = env::var(url_var).ok()?.trim().to_string();
        if url.is_empty() {
            return None;
        }
        Some(Arc::new(http_fiber_from_env(&url, token_var)))
    }

    pub struct TestContext {
        pub engine: Engine<HttpFiber>,
        pub fiber: Arc<HttpFiber>,
        pub seller: Option<Arc<HttpFiber>>,
        pub buyer: Option<Arc<HttpFiber>>,
    }

    impl TestContext {
        pub async fn new() -> Self {
            let node = FiberNode::from_env();
            node.require_ready().await;
            let seller = optional_peer("TWINE_E2E_SELLER_RPC", "TWINE_E2E_SELLER_TOKEN");
            let buyer = optional_peer("TWINE_E2E_BUYER_RPC", "TWINE_E2E_BUYER_TOKEN");
            if let Some(seller) = seller.as_ref() {
                seller
                    .node_pubkey()
                    .await
                    .expect("TWINE_E2E_SELLER_RPC is not reachable");
            }
            if let Some(buyer) = buyer.as_ref() {
                buyer
                    .node_pubkey()
                    .await
                    .expect("TWINE_E2E_BUYER_RPC is not reachable");
            }
            let db = Arc::new(Db::open_in_memory().expect("sqlite"));
            Self {
                engine: Engine::new(db, node.rpc.clone(), None),
                fiber: node.rpc,
                seller,
                buyer,
            }
        }

        pub fn require_seller(&self) -> &HttpFiber {
            self.seller
                .as_deref()
                .expect("TWINE_E2E_SELLER_RPC; run make e2e")
        }

        pub fn require_buyer(&self) -> &HttpFiber {
            self.buyer
                .as_deref()
                .expect("TWINE_E2E_BUYER_RPC; run make e2e")
        }
    }

    pub async fn open_order(engine: &Engine<HttpFiber>, seller: &str) -> String {
        let outbound = engine
            .handle(
                seller,
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "sell".into(),
                        available_ckb: "10".into(),
                        fiat_currency: "NGN".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
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
        ctx: &TestContext,
        buyer: &str,
        order_id: &str,
    ) -> (String, String, String) {
        let payout_node = ctx.buyer.as_ref().unwrap_or(&ctx.fiber);
        let payout = payout_node
            .new_payout_invoice(100_000_000, "twine payout", crate::MIN_TLC_EXPIRY_LIMIT_MS)
            .await
            .unwrap();
        let outbound = ctx
            .engine
            .handle(
                buyer,
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id: order_id.into(),
                        fiat_amount: "1000".into(),
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

    pub async fn pay_hold(ctx: &TestContext, hold_invoice: &str, payment_hash: &str) {
        ctx.require_seller()
            .send_payment(hold_invoice, crate::MAX_TLC_EXPIRY_LIMIT_MS)
            .await
            .expect("pay hold");
        wait_invoice_status(
            &ctx.fiber,
            payment_hash,
            "Received",
            Duration::from_secs(120),
        )
        .await;
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
