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
        })
    }
}
