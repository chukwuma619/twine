use super::{HttpFiber, hex_amount};
use crate::hex_u128;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Channel snapshot from Fiber `list_channels`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiberChannel {
    pub pubkey: String,
    pub state_name: String,
    pub local_balance: u128,
    pub offered_tlc_balance: u128,
    pub enabled: bool,
    pub is_one_way: bool,
    pub is_acceptor: bool,
    pub is_udt: bool,
    pub failure_detail: Option<String>,
}

/// Whether Twine can already pay the buyer, or a JIT open is still confirming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyerInbound {
    Ready,
    Opening,
}

#[async_trait]
pub trait ChannelRpc: Send + Sync {
    async fn list_channels(
        &self,
        pubkey: Option<&str>,
        only_pending: bool,
    ) -> Result<Vec<FiberChannel>>;
    async fn connect_peer(&self, pubkey: Option<&str>, address: Option<&str>) -> Result<()>;
    async fn list_peers(&self) -> Result<Vec<String>>;
    async fn open_channel(&self, pubkey: &str, funding_amount: u128) -> Result<String>;
}

#[async_trait]
impl ChannelRpc for HttpFiber {
    async fn list_channels(
        &self,
        pubkey: Option<&str>,
        only_pending: bool,
    ) -> Result<Vec<FiberChannel>> {
        let mut params = json!({});
        if let Some(pubkey) = pubkey.filter(|value| !value.trim().is_empty()) {
            params["pubkey"] = json!(pubkey);
        }
        if only_pending {
            params["only_pending"] = json!(true);
        }
        let result = self.call("list_channels", Some(params)).await?;
        parse_channels(&result)
    }

    async fn connect_peer(&self, pubkey: Option<&str>, address: Option<&str>) -> Result<()> {
        let address = address.map(str::trim).filter(|value| !value.is_empty());
        let pubkey = pubkey.map(str::trim).filter(|value| !value.is_empty());
        let params = match (address, pubkey) {
            (Some(address), Some(pubkey)) => {
                json!({ "address": address, "pubkey": pubkey, "save": true })
            }
            (Some(address), None) => json!({ "address": address, "save": true }),
            (None, Some(pubkey)) => json!({ "pubkey": pubkey }),
            (None, None) => bail!("connect_peer needs an address or a pubkey"),
        };
        self.call("connect_peer", Some(params)).await?;
        Ok(())
    }

    async fn list_peers(&self) -> Result<Vec<String>> {
        let result = self.call("list_peers", None).await?;
        parse_peers(&result)
    }

    async fn open_channel(&self, pubkey: &str, funding_amount: u128) -> Result<String> {
        let result = self
            .call(
                "open_channel",
                Some(json!({
                    "pubkey": pubkey,
                    "funding_amount": hex_u128(funding_amount),
                    "public": false,
                    "one_way": true,
                })),
            )
            .await?;
        result["temporary_channel_id"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("open_channel missing temporary_channel_id"))
    }
}

impl FiberChannel {
    pub fn usable_outbound(&self) -> u128 {
        if !self.enabled || self.is_udt || self.is_failed() {
            return 0;
        }
        if self.state_name != "ChannelReady" {
            return 0;
        }
        // One-way: only the initiator can send.
        if self.is_one_way && self.is_acceptor {
            return 0;
        }
        self.local_balance.saturating_sub(self.offered_tlc_balance)
    }

    pub fn is_pending(&self) -> bool {
        if self.is_failed() {
            return false;
        }
        matches!(
            self.state_name.as_str(),
            "NegotiatingFunding"
                | "CollaboratingFundingTx"
                | "SigningCommitment"
                | "AwaitingTxSignatures"
                | "AwaitingChannelReady"
        )
    }

    pub fn is_failed(&self) -> bool {
        if self
            .failure_detail
            .as_deref()
            .is_some_and(|detail| !detail.trim().is_empty())
        {
            return true;
        }
        matches!(self.state_name.as_str(), "Closed" | "Stale")
    }
}

pub fn outbound_covers(channels: &[FiberChannel], amount: u128) -> bool {
    channels
        .iter()
        .any(|channel| channel.usable_outbound() >= amount)
}

pub fn pending_will_cover(channels: &[FiberChannel], amount: u128) -> bool {
    channels.iter().any(|channel| {
        channel.is_pending()
            && !(channel.is_one_way && channel.is_acceptor)
            && channel
                .local_balance
                .saturating_sub(channel.offered_tlc_balance)
                >= amount
    })
}

pub fn has_live_open(channels: &[FiberChannel]) -> bool {
    channels.iter().any(FiberChannel::is_pending)
}

pub fn jit_funding_amount(trade_shannons: u128) -> u128 {
    trade_shannons
        .saturating_add(crate::CHANNEL_RESERVE_SHANNONS)
        .max(crate::CHANNEL_AUTO_ACCEPT_MIN_SHANNONS)
}

pub fn already_opening(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("already") || text.contains("pending") || text.contains("in progress")
}

pub fn already_connected(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("already") || text.contains("connected")
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

fn parse_peers(result: &Value) -> Result<Vec<String>> {
    let Some(peers) = result.get("peers").and_then(Value::as_array) else {
        bail!("list_peers missing peers");
    };
    Ok(peers
        .iter()
        .filter_map(|peer| peer.get("pubkey").and_then(Value::as_str))
        .map(str::trim)
        .filter(|pubkey| !pubkey.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn parse_channels(result: &Value) -> Result<Vec<FiberChannel>> {
    let Some(channels) = result.get("channels").and_then(Value::as_array) else {
        bail!("list_channels missing channels");
    };
    channels.iter().map(parse_channel).collect()
}

fn parse_channel(value: &Value) -> Result<FiberChannel> {
    let state_name = value
        .pointer("/state/state_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let failure = value
        .get("failure_detail")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
        .map(ToOwned::to_owned);
    Ok(FiberChannel {
        pubkey: value
            .get("pubkey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state_name,
        local_balance: hex_amount(value.get("local_balance"))?.unwrap_or(0),
        offered_tlc_balance: hex_amount(value.get("offered_tlc_balance"))?.unwrap_or(0),
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        is_one_way: value
            .get("is_one_way")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_acceptor: value
            .get("is_acceptor")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_udt: value
            .get("funding_udt_type_script")
            .is_some_and(|script| !script.is_null()),
        failure_detail: failure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready_json() -> Value {
        json!({
            "channels": [{
                "channel_id": "0x8cfd1718dace307c1c7eee756773fee9ded3bae8a3808fc207e078ac5f190413",
                "is_public": true,
                "is_acceptor": false,
                "is_one_way": false,
                "pubkey": "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71",
                "funding_udt_type_script": null,
                "state": { "state_name": "ChannelReady" },
                "local_balance": "0x9502f9000",
                "offered_tlc_balance": "0x0",
                "remote_balance": "0x38407b700",
                "enabled": true,
                "failure_detail": null
            }]
        })
    }

    #[test]
    fn reads_fiber_v091_list_peers() {
        let peers = parse_peers(&json!({
            "peers": [
                {
                    "pubkey": "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71",
                    "address": "/ip4/18.163.221.211/tcp/8119"
                }
            ]
        }))
        .unwrap();
        assert_eq!(
            peers[0],
            "02b6d4e3ab86a2ca2fad6fae0ecb2e1e559e0b911939872a90abdda6d20302be71"
        );
    }

    #[test]
    fn reads_fiber_v091_list_channels() {
        let channels = parse_channels(&ready_json()).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].state_name, "ChannelReady");
        assert_eq!(channels[0].local_balance, 40_000_000_000);
        assert_eq!(channels[0].usable_outbound(), 40_000_000_000);
        assert!(outbound_covers(&channels, 100_000_000));
        assert!(!channels[0].is_udt);
    }

    #[test]
    fn a_buyer_funded_one_way_channel_cannot_receive() {
        let mut channel = parse_channels(&ready_json()).unwrap().remove(0);
        channel.is_one_way = true;
        channel.is_acceptor = true;
        assert_eq!(channel.usable_outbound(), 0);
        assert!(!outbound_covers(&[channel], 1));
    }

    #[test]
    fn operator_funded_one_way_can_pay() {
        let mut channel = parse_channels(&ready_json()).unwrap().remove(0);
        channel.is_one_way = true;
        channel.is_acceptor = false;
        channel.local_balance = 100_000_000;
        assert_eq!(channel.usable_outbound(), 100_000_000);
    }

    #[test]
    fn pending_open_is_not_ready() {
        let pending = FiberChannel {
            pubkey: "02ab".into(),
            state_name: "AwaitingChannelReady".into(),
            local_balance: 100_000_000,
            offered_tlc_balance: 0,
            enabled: true,
            is_one_way: true,
            is_acceptor: false,
            is_udt: false,
            failure_detail: None,
        };
        assert!(pending.is_pending());
        assert_eq!(pending.usable_outbound(), 0);
        assert!(pending_will_cover(&[pending.clone()], 100_000_000));
        assert!(!pending_will_cover(&[pending.clone()], 100_000_001));
        assert!(has_live_open(&[pending]));
    }

    #[test]
    fn split_channels_do_not_cover_one_payment() {
        let left = FiberChannel {
            pubkey: "02ab".into(),
            state_name: "ChannelReady".into(),
            local_balance: 60_000_000,
            offered_tlc_balance: 0,
            enabled: true,
            is_one_way: true,
            is_acceptor: false,
            is_udt: false,
            failure_detail: None,
        };
        let right = FiberChannel {
            local_balance: 40_000_000,
            ..left.clone()
        };
        assert!(!outbound_covers(&[left.clone(), right], 100_000_000));
        assert!(outbound_covers(&[left], 60_000_000));
    }

    #[test]
    fn a_buyer_funded_pending_channel_will_not_pay() {
        let pending = FiberChannel {
            pubkey: "02ab".into(),
            state_name: "NegotiatingFunding".into(),
            local_balance: 500_000_000,
            offered_tlc_balance: 0,
            enabled: true,
            is_one_way: true,
            is_acceptor: true,
            is_udt: false,
            failure_detail: None,
        };
        assert!(pending.is_pending());
        assert!(!pending_will_cover(&[pending], 1));
    }

    #[test]
    fn failed_open_is_not_live() {
        let failed = FiberChannel {
            pubkey: "02ab".into(),
            state_name: "NegotiatingFunding".into(),
            local_balance: 0,
            offered_tlc_balance: 0,
            enabled: false,
            is_one_way: true,
            is_acceptor: false,
            is_udt: false,
            failure_detail: Some("peer timed out".into()),
        };
        assert!(failed.is_failed());
        assert!(!failed.is_pending());
        assert!(!has_live_open(&[failed]));
    }

    #[test]
    fn funding_adds_fiber_channel_reserve() {
        assert_eq!(jit_funding_amount(100_000_000), 10_000_000_000);
        assert_eq!(jit_funding_amount(1), 10_000_000_000);
        assert_eq!(jit_funding_amount(5_000_000_000), 14_900_000_000);
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
