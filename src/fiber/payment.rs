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
        })
    }
}

fn preimage_field(result: &serde_json::Value) -> Option<String> {
    result
        .get("payment_preimage")
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
}
