//! Preimage registration Fiber's watchtower uses to claim a hold on force-close.

use super::HttpFiber;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait WatchtowerRpc: Send + Sync {
    async fn create_preimage(&self, payment_hash: &str, preimage: &str) -> Result<()>;
    async fn remove_preimage(&self, payment_hash: &str) -> Result<()>;
}

#[async_trait]
impl WatchtowerRpc for HttpFiber {
    async fn create_preimage(&self, payment_hash: &str, preimage: &str) -> Result<()> {
        self.call(
            "create_preimage",
            Some(json!({
                "payment_hash": payment_hash,
                "preimage": preimage,
            })),
        )
        .await?;
        Ok(())
    }

    async fn remove_preimage(&self, payment_hash: &str) -> Result<()> {
        self.call(
            "remove_preimage",
            Some(json!({ "payment_hash": payment_hash })),
        )
        .await?;
        Ok(())
    }
}
