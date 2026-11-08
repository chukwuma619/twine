use crate::types::{Outbound, PublicOrder, TradeRecord};
use crate::{
    Envelope, FIBER_NODE_TAG, KIND_ACTION, KIND_FIBER_NODE, KIND_ORDER, KIND_TRADE_RECORD,
};
use anyhow::{Context, Result, bail};
use nostr::nips::nip44::{self, Version};
use nostr_sdk::prelude::*;
use std::collections::BTreeSet;
use std::time::Duration;
use tracing::{info, warn};

const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_RETRY: Duration = Duration::from_secs(5);
const RELAY_OK_TIMEOUT: Duration = Duration::from_secs(4);
const PUBLISH_ATTEMPTS: u32 = 5;
const PUBLISH_RETRY: Duration = Duration::from_secs(5);
const ACTION_SUBSCRIPTION: &str = "twine-actions";

/// `disconnect_relay` reports Terminated while teardown is still in flight.
/// A connect in that window is a no-op and the relay stays down.
const RECONNECT_GRACE: Duration = Duration::from_millis(250);

pub async fn connect(keys: &Keys, relays: &[String]) -> Result<Client> {
    let client = Client::builder()
        .authenticator(SignerAuthenticator::new(keys.clone()))
        .build();
    for relay in relays {
        client
            .add_relay(relay)
            .retry_interval(RELAY_RETRY)
            .adjust_retry_interval(false)
            .await
            .with_context(|| format!("invalid relay {relay}"))?;
    }
    client.connect().and_wait(RELAY_CONNECT_TIMEOUT).await;

    let mut connected = 0usize;
    for (url, relay) in client.relays().await {
        let status = relay.status();
        if status.is_connected() {
            connected += 1;
            info!(%url, "nostr relay connected");
        } else {
            warn!(%url, %status, "nostr relay not connected");
        }
    }
    if connected == 0 {
        warn!("no nostr relay connected yet; the pool will keep retrying");
    }
    Ok(client)
}

pub fn action_subscription_id() -> SubscriptionId {
    SubscriptionId::new(ACTION_SUBSCRIPTION)
}

/// Relays whose connection task is sleeping its retry interval.
/// `Client::connect` does not wake these. It only starts a task when the
/// status is Initialized, Terminated, or Sleeping.
pub fn disconnected_relay_urls(
    relays: impl IntoIterator<Item = (String, RelayStatus)>,
) -> Vec<String> {
    relays
        .into_iter()
        .filter(|(_, status)| *status == RelayStatus::Disconnected)
        .map(|(url, _)| url)
        .collect()
}

/// Bounce every disconnected relay so the next subscribe is not waiting
/// behind a 5–60s SDK retry sleep.
pub async fn wake_disconnected(client: &Client) -> usize {
    let urls = disconnected_relay_urls(
        client
            .relays()
            .await
            .into_iter()
            .map(|(url, relay)| (url.to_string(), relay.status())),
    );
    for url in &urls {
        if let Err(error) = client.disconnect_relay(url.as_str()).await {
            warn!(%url, %error, "disconnect before wake failed");
        }
    }
    if !urls.is_empty() {
        tokio::time::sleep(RECONNECT_GRACE).await;
    }
    for url in &urls {
        if let Err(error) = client.connect_relay(url.as_str()).await {
            warn!(%url, %error, "wake did not start");
        }
    }
    urls.len()
}

pub async fn connected_relay_urls(client: &Client) -> BTreeSet<String> {
    let mut urls = BTreeSet::new();
    for (url, relay) in client.relays().await {
        if relay.status().is_connected() {
            urls.insert(url.to_string());
        }
    }
    urls
}

fn is_action_filter(filter: &Filter, daemon: &PublicKey) -> bool {
    filter
        .kinds
        .as_ref()
        .is_some_and(|kinds| kinds.contains(&Kind::from(KIND_ACTION)))
        && filter.extract_public_keys().contains(daemon)
}

/// Subscribes any connected relay that is missing the action filter.
pub async fn ensure_action_subscription(client: &Client, daemon: PublicKey) -> Result<()> {
    let id = action_subscription_id();
    let existing = client.subscriptions().await;
    let covered = existing.get(&id);
    let mut missing = Vec::new();
    for (url, relay) in client.relays().await {
        if !relay.status().is_connected() {
            continue;
        }
        let ready = covered.is_some_and(|by_relay| {
            by_relay.get(&url).is_some_and(|filters| {
                filters
                    .iter()
                    .any(|filter| is_action_filter(filter, &daemon))
            })
        });
        if !ready {
            missing.push(url);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let filter = action_filter(daemon);
    let target = ReqTarget::manual(
        missing
            .iter()
            .map(|url| (url.clone(), vec![filter.clone()])),
    );
    let output = client.subscribe(target).with_id(id).await?;
    for (url, error) in &output.failed {
        warn!(%url, %error, "action subscription failed");
    }
    if output.success.is_empty() {
        bail!("no relay accepted the action subscription");
    }
    for url in output.success.keys() {
        info!(%url, "subscribed to actions");
    }
    Ok(())
}

pub fn action_filter(daemon: PublicKey) -> Filter {
    Filter::new().kind(Kind::from(KIND_ACTION)).pubkey(daemon)
}

pub fn decrypt_action(keys: &Keys, event: &Event) -> Result<Envelope> {
    event.verify()?;
    let plaintext = nip44::decrypt(keys.secret_key(), &event.pubkey, event.content.as_str())?;
    Ok(serde_json::from_str(&plaintext)?)
}

pub async fn publish_outbounds(client: &Client, keys: &Keys, outbound: &[Outbound]) -> Result<()> {
    for item in outbound {
        match item {
            Outbound::Reply { to, envelope } => {
                let recipient = PublicKey::parse(to)?;
                let json = serde_json::to_string(envelope)?;
                let ciphertext = nip44::encrypt(keys.secret_key(), &recipient, json, Version::V2)?;
                let event = EventBuilder::new(Kind::from(KIND_ACTION), ciphertext)
                    .tag(Tag::public_key(recipient))
                    .finalize(keys)?;
                publish_event(client, &event).await?;
            }
            Outbound::PublicOrder(order) => {
                let event = public_order_event(keys, order)?;
                publish_event(client, &event).await?;
            }
            Outbound::TradeRecord(record) => {
                let event = trade_record_event(keys, record)?;
                publish_event(client, &event).await?;
            }
        }
    }
    Ok(())
}

pub fn trade_record_event(keys: &Keys, record: &TradeRecord) -> Result<Event> {
    Ok(EventBuilder::new(
        Kind::from(KIND_TRADE_RECORD),
        serde_json::to_string(record)?,
    )
    .tag(Tag::identifier(&record.trade_id))
    .finalize(keys)?)
}

pub fn public_order_event(keys: &Keys, order: &PublicOrder) -> Result<Event> {
    Ok(
        EventBuilder::new(Kind::from(KIND_ORDER), serde_json::to_string(order)?)
            .tag(Tag::identifier(&order.order_id))
            .finalize(keys)?,
    )
}

/// Relays that stored this one attempt. Empty means none did.
pub async fn accepted_relays(client: &Client, event: &Event) -> Result<BTreeSet<String>> {
    let output = client
        .send_event(event)
        .ok_timeout(RELAY_OK_TIMEOUT)
        .await?;
    for (url, error) in &output.failed {
        warn!(%url, %error, "relay rejected event");
    }
    if output.success.is_empty() {
        bail!("no relay accepted the event");
    }
    Ok(output.success.keys().map(|url| url.to_string()).collect())
}

/// Relays that accepted every publish so far. A relay that missed one stays out.
pub fn relays_with_everything(
    so_far: Option<BTreeSet<String>>,
    accepted: BTreeSet<String>,
) -> BTreeSet<String> {
    match so_far {
        Some(prev) => prev.intersection(&accepted).cloned().collect(),
        None => accepted,
    }
}

/// Retries until one relay accepts the event. A relay that already stored it treats the retry as a duplicate.
pub async fn publish_event(client: &Client, event: &Event) -> Result<()> {
    for attempt in 1..=PUBLISH_ATTEMPTS {
        let output = client
            .send_event(event)
            .ok_timeout(RELAY_OK_TIMEOUT)
            .await?;
        for (url, error) in &output.failed {
            warn!(%url, %error, attempt, "relay rejected event");
        }
        if !output.success.is_empty() {
            for url in output.success.keys() {
                info!(%url, attempt, "relay accepted event");
            }
            return Ok(());
        }
        warn!(attempt, "no relay accepted the event");
        if attempt < PUBLISH_ATTEMPTS {
            tokio::time::sleep(PUBLISH_RETRY).await;
        }
    }
    bail!("no relay accepted the event")
}

pub fn sender_hex(event: &Event) -> String {
    event.pubkey.to_hex()
}

pub fn fiber_node_event(
    keys: &Keys,
    pubkey: &str,
    solver: Option<&str>,
    addresses: &[String],
) -> Result<Event> {
    let pubkey = pubkey.trim();
    if pubkey.is_empty() {
        bail!("fiber node pubkey is empty");
    }
    let solver = solver.map(str::trim).filter(|value| !value.is_empty());
    let addresses: Vec<&str> = addresses
        .iter()
        .map(|addr| addr.trim())
        .filter(|addr| !addr.is_empty())
        .collect();
    let content = serde_json::json!({
        "pubkey": pubkey,
        "solver": solver,
        "solver_window_secs": solver.map(|_| crate::SOLVER_WINDOW_SECS),
        "addresses": addresses,
    })
    .to_string();
    Ok(EventBuilder::new(Kind::from(KIND_FIBER_NODE), content)
        .tag(Tag::identifier(FIBER_NODE_TAG))
        .finalize(keys)?)
}

pub async fn publish_fiber_node(
    client: &Client,
    keys: &Keys,
    pubkey: &str,
    solver: Option<&str>,
    addresses: &[String],
) -> Result<()> {
    let event = fiber_node_event(keys, pubkey, solver, addresses)?;
    publish_event(client, &event).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relays_with_everything_keeps_only_the_overlap() {
        let both = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let second = BTreeSet::from(["b".to_string(), "c".to_string()]);
        let first = relays_with_everything(None, both);
        assert_eq!(
            relays_with_everything(Some(first), second),
            BTreeSet::from(["b".to_string()])
        );
    }

    #[test]
    fn disconnected_relays_are_the_ones_connect_cannot_wake() {
        assert_eq!(
            disconnected_relay_urls([
                ("wss://a".into(), RelayStatus::Connected),
                ("wss://b".into(), RelayStatus::Disconnected),
                ("wss://c".into(), RelayStatus::Terminated),
            ]),
            vec!["wss://b".to_string()]
        );
    }

    #[test]
    fn action_filter_matches_only_the_daemon_inbox() {
        let daemon = Keys::generate().public_key();
        let other = Keys::generate().public_key();
        let filter = action_filter(daemon);
        assert!(is_action_filter(&filter, &daemon));
        assert!(!is_action_filter(&filter, &other));
        assert!(!is_action_filter(
            &Filter::new().kind(Kind::from(KIND_ORDER)).pubkey(daemon),
            &daemon
        ));
    }

    #[test]
    fn fiber_node_event_is_signed_by_the_daemon() {
        let keys = Keys::generate();
        let event = fiber_node_event(&keys, " 02abc ", None, &[]).unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind, Kind::from(KIND_FIBER_NODE));
        assert!(event.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.len() >= 2 && values[0] == "d" && values[1] == FIBER_NODE_TAG
        }));
        let body: serde_json::Value = serde_json::from_str(event.content.as_str()).unwrap();
        assert_eq!(body["pubkey"], "02abc");
        assert!(body["solver"].is_null());
        assert!(body["solver_window_secs"].is_null());
        let with_solver = fiber_node_event(
            &keys,
            "02abc",
            Some("solverpk"),
            &["/ip4/1.2.3.4/tcp/8228".into()],
        )
        .unwrap();
        let solved: serde_json::Value = serde_json::from_str(with_solver.content.as_str()).unwrap();
        assert_eq!(solved["solver"], "solverpk");
        assert_eq!(solved["solver_window_secs"], crate::SOLVER_WINDOW_SECS);
        assert_eq!(solved["addresses"][0], "/ip4/1.2.3.4/tcp/8228");
        assert!(fiber_node_event(&keys, " ", None, &[]).is_err());
    }
}
