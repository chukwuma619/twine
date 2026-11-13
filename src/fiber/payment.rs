use super::{HttpFiber, PaymentInfo};
use crate::{MAX_FEE_RATE, hex_u64, hex_u128};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait PaymentRpc: Send + Sync {
    async fn send_payment(&self, invoice: &str, tlc_expiry_limit_ms: u64) -> Result<PaymentInfo>;
    /// Route check only. Fiber `send_payment` with `dry_run: true` builds a path,
    /// returns `Created`, and does not store a session or send an onion, so it cannot
    /// reveal the preimage. `build_router` is the wrong call: it requires `hops_info`.
    async fn probe_payment(&self, invoice: &str, tlc_expiry_limit_ms: u64) -> Result<PaymentInfo>;
    async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo>;
}

#[async_trait]
impl PaymentRpc for HttpFiber {
    async fn send_payment(&self, invoice: &str, tlc_expiry_limit_ms: u64) -> Result<PaymentInfo> {
        let result = self
            .call(
                "send_payment",
                Some(send_payment_params(invoice, tlc_expiry_limit_ms, false)?),
            )
            .await?;
        payment_from_send(result)
    }

    async fn probe_payment(&self, invoice: &str, tlc_expiry_limit_ms: u64) -> Result<PaymentInfo> {
        let result = self
            .call(
                "send_payment",
                Some(probe_payment_params(invoice, tlc_expiry_limit_ms)?),
            )
            .await?;
        payment_from_send(result)
    }

    async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo> {
        let result = self
            .call("get_payment", Some(json!({ "payment_hash": payment_hash })))
            .await?;
        Ok(PaymentInfo {
            payment_hash: payment_hash.to_string(),
            status: result["status"]
                .as_str()
                .ok_or_else(|| anyhow!("get_payment missing status"))?
                .to_string(),
            preimage: preimage_field(&result),
            failed_error: failed_error_field(&result),
        })
    }
}

/// Params for Fiber `send_payment` `dry_run`. A missing path comes back as an
/// RPC error whose message contains `Failed to build route`, not as a stored payment.
pub fn probe_payment_params(invoice: &str, tlc_expiry_limit_ms: u64) -> Result<serde_json::Value> {
    send_payment_params(invoice, tlc_expiry_limit_ms, true)
}

/// An invoice with a trampoline route hint is paid by fields through the hinted node,
/// which Fiber supports although it cannot parse attr 10 yet. Trampoline routing needs
/// `max_fee_amount`; it gets the same cap `max_fee_rate` gives a plain payment.
pub fn send_payment_params(
    invoice: &str,
    tlc_expiry_limit_ms: u64,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let invoice = super::hint::bare_invoice(invoice);
    let mut params = match super::hint::decode_hinted_invoice(invoice) {
        Ok(hinted) => {
            let Some(amount) = hinted.amount else {
                bail!("trampoline invoice is missing an amount");
            };
            let mut fields = json!({
                "target_pubkey": hinted.payee,
                "amount": hex_u128(amount),
                "payment_hash": hinted.payment_hash,
                "trampoline_hops": [hinted.trampoline_hint],
                "max_fee_amount": hex_u128(amount * u128::from(MAX_FEE_RATE) / 1_000),
                "max_fee_rate": hex_u64(MAX_FEE_RATE),
                "tlc_expiry_limit": hex_u64(tlc_expiry_limit_ms),
            });
            if let Some(delta) = hinted.final_tlc_expiry_delta_ms {
                fields["final_tlc_expiry_delta"] = json!(hex_u64(delta));
            }
            if hinted.hash_algorithm == 1 {
                fields["hash_algorithm"] = json!("sha256");
            }
            fields
        }
        Err(_) => json!({
            "invoice": invoice,
            "max_fee_rate": hex_u64(MAX_FEE_RATE),
            "tlc_expiry_limit": hex_u64(tlc_expiry_limit_ms),
        }),
    };
    if dry_run {
        params["dry_run"] = json!(true);
    }
    Ok(params)
}

fn payment_from_send(result: serde_json::Value) -> Result<PaymentInfo> {
    Ok(PaymentInfo {
        payment_hash: result["payment_hash"]
            .as_str()
            .ok_or_else(|| anyhow!("send_payment missing payment_hash"))?
            .to_string(),
        status: result["status"].as_str().unwrap_or("Created").to_string(),
        preimage: preimage_field(&result),
        failed_error: failed_error_field(&result),
    })
}

fn preimage_field(result: &serde_json::Value) -> Option<String> {
    text_field(result, "payment_preimage")
}

fn failed_error_field(result: &serde_json::Value) -> Option<String> {
    text_field(result, "failed_error")
}

pub fn route_not_ready(error: &anyhow::Error) -> bool {
    route_failure_text(&error.to_string())
}

/// A stored `Failed` payment can be sent again when Fiber had no path.
/// An expired invoice refunds the hold. A hash mismatch stays a real failure.
pub fn payment_route_failed(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(route_failure_text)
}

pub fn payment_invoice_expired(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(|error| error.to_ascii_lowercase().contains("invoice expired"))
}

pub fn payment_expiry_limited(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(|error| error.to_ascii_lowercase().contains("tlc_expiry_limit"))
}

pub fn payment_session_exists(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(|error| {
        error
            .to_ascii_lowercase()
            .contains("payment session already exists")
    })
}

pub fn payment_attempts_inflight(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(|error| {
        error
            .to_ascii_lowercase()
            .contains("attempts that are in flight state")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendFailureKind {
    InvoiceExpired,
    SessionExists,
    AttemptsInflight,
    ExpiryLimited,
    NoRoute,
    Other,
}

pub fn classify_send_failure(failed_error: Option<&str>) -> SendFailureKind {
    if payment_invoice_expired(failed_error) {
        SendFailureKind::InvoiceExpired
    } else if payment_session_exists(failed_error) {
        SendFailureKind::SessionExists
    } else if payment_attempts_inflight(failed_error) {
        SendFailureKind::AttemptsInflight
    } else if payment_expiry_limited(failed_error) {
        SendFailureKind::ExpiryLimited
    } else if payment_route_failed(failed_error) {
        SendFailureKind::NoRoute
    } else {
        SendFailureKind::Other
    }
}

fn route_failure_text(error: &str) -> bool {
    if payment_expiry_limited(Some(error)) {
        return false;
    }
    let text = error.to_ascii_lowercase();
    text.contains("failed to build route")
        || text.contains("no route")
        || text.contains("no path")
        || text.contains("route not found")
        || text.contains("insufficient balance")
}

fn text_field(result: &serde_json::Value, key: &str) -> Option<String> {
    result
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_asks_fiber_not_to_send() {
        let params = probe_payment_params("invoice", 60_000).unwrap();
        assert_eq!(params["dry_run"], true);
        assert_eq!(params["invoice"], "invoice");
        assert!(params.get("keysend").is_none());
    }

    #[test]
    fn trampoline_payment_requires_an_amount() {
        let mut payee = [0x22u8; 33];
        payee[0] = 0x02;
        let mut trampoline = [0x33u8; 33];
        trampoline[0] = 0x03;
        let invoice = super::super::hint::test_invoice::encode_hinted_invoice(
            "fibt",
            1_000,
            &[0x11; 32],
            None,
            Some(86_400_000),
            &payee,
            0,
            &trampoline,
        );
        let err = send_payment_params(&invoice, 60_000, false).unwrap_err();
        assert!(err.to_string().contains("missing an amount"));
    }

    #[test]
    fn trampoline_payment_uses_fields_and_fee_cap() {
        let mut payee = [0x22u8; 33];
        payee[0] = 0x02;
        let mut trampoline = [0x33u8; 33];
        trampoline[0] = 0x03;
        let invoice = super::super::hint::test_invoice::encode_hinted_invoice(
            "fibt10000000000",
            1_000,
            &[0x11; 32],
            None,
            Some(86_400_000),
            &payee,
            0,
            &trampoline,
        );
        let params = send_payment_params(&invoice, 60_000, true).unwrap();
        assert_eq!(params["dry_run"], true);
        assert_eq!(params["target_pubkey"], hex::encode(payee));
        assert_eq!(params["trampoline_hops"], json!([hex::encode(trampoline)]));
        assert_eq!(params["amount"], "0x2540be400");
        assert_eq!(params["max_fee_amount"], "0x2faf080");
        assert!(params.get("invoice").is_none());
    }

    #[test]
    fn payment_preimage_is_the_field_fiber_returns() {
        assert_eq!(
            preimage_field(&json!({"payment_preimage": " 0xabc "})).as_deref(),
            Some("0xabc")
        );
        assert!(preimage_field(&json!({"status": "Success"})).is_none());
        assert!(preimage_field(&json!({"payment_preimage": ""})).is_none());
    }

    #[test]
    fn failed_error_is_kept_for_a_route_retry() {
        assert_eq!(
            failed_error_field(&json!({
                "failed_error": " Build payment route error: PathFind error: no path found "
            }))
            .as_deref(),
            Some("Build payment route error: PathFind error: no path found")
        );
        assert!(failed_error_field(&json!({"status": "Failed"})).is_none());
    }

    #[test]
    fn a_missing_path_can_be_retried() {
        assert!(payment_route_failed(Some(
            "Build payment route error: Failed to build route, PathFind error: no path found"
        )));
        assert!(payment_route_failed(Some(
            "Insufficient balance: max outbound liquidity"
        )));
        assert!(!payment_route_failed(Some("invoice expired")));
        assert!(payment_invoice_expired(Some("invoice expired")));
        assert!(!payment_invoice_expired(Some(
            "Build payment route error: PathFind error: no path found"
        )));
        assert!(!payment_route_failed(None));
        assert!(!payment_route_failed(Some(
            "Failed to build route, payment hop expiry 10 exceeds tlc_expiry_limit 9"
        )));
        assert!(payment_expiry_limited(Some(
            "tlc_expiry_limit is too small, final_tlc_expiry_delta: 1, tlc_expiry_limit: 0"
        )));
        assert!(payment_session_exists(Some(
            "Payment session already exists: 0xabc with payment session status: Inflight"
        )));
        assert!(!payment_session_exists(Some(
            "Build payment route error: PathFind error: no path found"
        )));
        assert!(payment_attempts_inflight(Some(
            "Payment session 0xabc has attempts that are in flight state, can not retry"
        )));
        assert!(!payment_attempts_inflight(Some(
            "Payment session already exists: 0xabc with payment session status: Inflight"
        )));
        assert_eq!(
            classify_send_failure(Some(
                "Payment session already exists: 0xabc with payment session status: Inflight"
            )),
            SendFailureKind::SessionExists
        );
        assert_eq!(
            classify_send_failure(Some(
                "Payment session 0xabc has attempts that are in flight state, can not retry"
            )),
            SendFailureKind::AttemptsInflight
        );
        assert_eq!(
            classify_send_failure(Some(
                "Build payment route error: PathFind error: no path found"
            )),
            SendFailureKind::NoRoute
        );
    }
}
