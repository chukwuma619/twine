use super::HttpFiber;
use crate::fiber_addr_is_reachable;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiberNodeInfo {
    pub pubkey: String,
    pub addresses: Vec<String>,
}

#[async_trait]
pub trait NodeRpc: Send + Sync {
    async fn node_info(&self) -> Result<FiberNodeInfo>;
    async fn node_pubkey(&self) -> Result<String> {
        Ok(self.node_info().await?.pubkey)
    }
}

#[async_trait]
impl NodeRpc for HttpFiber {
    async fn node_info(&self) -> Result<FiberNodeInfo> {
        parse_node_info(&self.call("node_info", None).await?)
    }
}

fn parse_node_info(result: &Value) -> Result<FiberNodeInfo> {
    let pubkey = result["pubkey"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("node_info missing pubkey"))?
        .to_string();
    let addresses = result
        .get("addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|addr| fiber_addr_is_reachable(addr))
        .map(ToOwned::to_owned)
        .collect();
    Ok(FiberNodeInfo { pubkey, addresses })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_info_keeps_public_addresses() {
        let info = parse_node_info(&json!({
            "pubkey": "02abc",
            "addresses": [
                "/ip4/0.0.0.0/tcp/8228",
                "/ip4/127.0.0.1/tcp/8228",
                "/ip4/203.0.113.10/tcp/8228"
            ]
        }))
        .unwrap();
        assert_eq!(info.pubkey, "02abc");
        assert_eq!(info.addresses, vec!["/ip4/203.0.113.10/tcp/8228"]);
        let with_peer = parse_node_info(&json!({
            "pubkey": "02abc",
            "addresses": ["/ip4/203.0.113.10/tcp/8230/p2p/Qmabc"]
        }))
        .unwrap();
        assert_eq!(
            with_peer.addresses,
            vec!["/ip4/203.0.113.10/tcp/8230/p2p/Qmabc"]
        );
    }
}
