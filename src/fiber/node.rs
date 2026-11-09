use super::{HttpFiber, same_hash};
use crate::fiber_addr_is_reachable;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiberNodeInfo {
    pub pubkey: String,
    pub addresses: Vec<String>,
}

/// Live inbound hold TLC and the channel delay used to claim it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundHold {
    pub expires_at_ms: i64,
    pub claim_margin_ms: u64,
}

#[async_trait]
pub trait NodeRpc: Send + Sync {
    async fn node_info(&self) -> Result<FiberNodeInfo>;
    async fn node_pubkey(&self) -> Result<String> {
        Ok(self.node_info().await?.pubkey)
    }
    async fn inbound_hold(&self, payment_hash: &str) -> Result<Option<InboundHold>>;
}

#[async_trait]
impl NodeRpc for HttpFiber {
    async fn node_info(&self) -> Result<FiberNodeInfo> {
        parse_node_info(&self.call("node_info", None).await?)
    }

    async fn inbound_hold(&self, payment_hash: &str) -> Result<Option<InboundHold>> {
        let result = self
            .call(
                "list_channels",
                Some(serde_json::json!({ "include_closed": true })),
            )
            .await?;
        Ok(earliest_inbound_hold(&result, payment_hash))
    }
}

fn earliest_inbound_hold(channels: &Value, payment_hash: &str) -> Option<InboundHold> {
    let mut best: Option<(i64, Option<u64>)> = None;
    let rows = channels.get("channels").and_then(Value::as_array)?;
    for channel in rows {
        let channel_delta = channel_tlc_expiry_delta_ms(channel);
        let Some(tlcs) = channel.get("pending_tlcs").and_then(Value::as_array) else {
            continue;
        };
        for tlc in tlcs {
            if !same_hash(
                tlc.get("payment_hash")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                payment_hash,
            ) {
                continue;
            }
            if tlc
                .get("forwarding_channel_id")
                .and_then(Value::as_str)
                .is_some()
            {
                continue;
            }
            if !inbound_still_held(tlc.get("status")) {
                continue;
            }
            let Some(expiry) = tlc.get("expiry").and_then(hex_i64) else {
                continue;
            };
            let replace = best.is_none_or(|(current, _)| expiry < current);
            if replace {
                best = Some((expiry, channel_delta));
            }
        }
    }
    best.map(|(expires_at_ms, channel_delta)| InboundHold {
        expires_at_ms,
        claim_margin_ms: crate::claim_margin_ms(channel_delta),
    })
}

fn channel_tlc_expiry_delta_ms(channel: &Value) -> Option<u64> {
    channel.get("tlc_expiry_delta").and_then(hex_or_u64)
}

fn inbound_still_held(status: Option<&Value>) -> bool {
    let Some(state) = status
        .and_then(|status| status.get("Inbound"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    !matches!(state, "LocalRemoved" | "RemoveAckConfirmed")
}

fn hex_i64(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    i64::from_str_radix(digits, 16).ok()
}

fn hex_or_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let text = value.as_str()?;
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u64::from_str_radix(digits, 16).ok()
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

    #[test]
    fn inbound_hold_keeps_the_earliest_live_tlc_and_channel_delay() {
        let channels = json!({
            "channels": [{
                "tlc_expiry_delta": "0x1b77400",
                "pending_tlcs": [
                    {
                        "payment_hash": "0xabc",
                        "expiry": "0x14",
                        "status": {"Outbound": "Committed"}
                    },
                    {
                        "payment_hash": "0xAbC",
                        "expiry": "0x64",
                        "forwarding_channel_id": "0x01",
                        "status": {"Inbound": "Committed"}
                    },
                    {
                        "payment_hash": "0xabc",
                        "expiry": "0x3c",
                        "forwarding_channel_id": null,
                        "status": {"Inbound": "LocalRemoved"}
                    },
                    {
                        "payment_hash": "0xabc",
                        "expiry": "0x50",
                        "status": {"Inbound": "Committed"}
                    },
                    {
                        "payment_hash": "0xabc",
                        "expiry": "0x28",
                        "status": {"Inbound": "RemoteAnnounced"}
                    }
                ]
            }]
        });
        let hold = earliest_inbound_hold(&channels, "0xabc").unwrap();
        assert_eq!(hold.expires_at_ms, 0x28);
        assert_eq!(
            hold.claim_margin_ms,
            crate::claim_margin_ms(Some(0x1b77400))
        );
        assert!(earliest_inbound_hold(&channels, "0xother").is_none());
    }
}
