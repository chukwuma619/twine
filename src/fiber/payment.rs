use super::{HttpFiber, PaymentInfo};
use crate::{MAX_FEE_RATE, hex_u64};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait PaymentRpc: Send + Sync {
    async fn send_payment(&self, invoice: &str) -> Result<PaymentInfo>;
    async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo>;
}

#[async_trait]
impl PaymentRpc for HttpFiber {
    async fn send_payment(&self, invoice: &str) -> Result<PaymentInfo> {
        let result = self
            .call(
                "send_payment",
                Some(json!({
                    "invoice": invoice,
                    "max_fee_rate": hex_u64(MAX_FEE_RATE),
                })),
            )
            .await?;
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
/// An expired invoice or a hash mismatch stays a real failure.
pub fn payment_route_failed(failed_error: Option<&str>) -> bool {
    failed_error.is_some_and(route_failure_text)
}

fn route_failure_text(error: &str) -> bool {
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
        assert!(!payment_route_failed(None));
    }
}
