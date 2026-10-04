use super::HttpFiber;
use anyhow::{Result, anyhow};
use async_trait::async_trait;

#[async_trait]
pub trait NodeRpc: Send + Sync {
    async fn node_pubkey(&self) -> Result<String>;
}

#[async_trait]
impl NodeRpc for HttpFiber {
    async fn node_pubkey(&self) -> Result<String> {
        let result = self.call("node_info", None).await?;
        result["pubkey"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("node_info missing pubkey"))
    }
}
