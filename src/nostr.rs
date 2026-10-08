use crate::types::{Outbound, PublicOrder, SupportedPaymentMethod};
use crate::{
    Envelope, FIBER_NODE_TAG, KIND_ACTION, KIND_CATALOG, KIND_FIBER_NODE, KIND_ORDER,
    PAYMENT_CATALOG_TAG,
};
use anyhow::{Context, Result, bail};
use nostr::nips::nip44::{self, Version};
use nostr_sdk::prelude::*;
use std::time::Duration;
use tracing::{info, warn};

const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_OK_TIMEOUT: Duration = Duration::from_secs(4);
const PUBLISH_ATTEMPTS: u32 = 5;
const PUBLISH_RETRY: Duration = Duration::from_secs(5);

pub async fn connect(keys: &Keys, relays: &[String]) -> Result<Client> {
    let client = Client::builder()
        .authenticator(SignerAuthenticator::new(keys.clone()))
        .build();
    for relay in relays {
        client
            .add_relay(relay)
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
        bail!("no nostr relay connected");
    }
    Ok(client)
}

pub async fn subscribe_actions(client: &Client, daemon: PublicKey) -> Result<()> {
    let output = client.subscribe(action_filter(daemon)).await?;
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
        }
    }
    Ok(())
}

pub fn public_order_event(keys: &Keys, order: &PublicOrder) -> Result<Event> {
    Ok(
        EventBuilder::new(Kind::from(KIND_ORDER), serde_json::to_string(order)?)
            .tag(Tag::identifier(&order.order_id))
            .finalize(keys)?,
    )
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

pub fn fiber_node_event(keys: &Keys, pubkey: &str, solver: Option<&str>) -> Result<Event> {
    let pubkey = pubkey.trim();
    if pubkey.is_empty() {
        bail!("fiber node pubkey is empty");
    }
    let solver = solver.map(str::trim).filter(|value| !value.is_empty());
    let content = serde_json::json!({ "pubkey": pubkey, "solver": solver }).to_string();
    Ok(EventBuilder::new(Kind::from(KIND_FIBER_NODE), content)
        .tag(Tag::identifier(FIBER_NODE_TAG))
        .finalize(keys)?)
}

pub async fn publish_fiber_node(
    client: &Client,
    keys: &Keys,
    pubkey: &str,
    solver: Option<&str>,
) -> Result<()> {
    let event = fiber_node_event(keys, pubkey, solver)?;
    publish_event(client, &event).await
}

pub fn payment_catalog_event(keys: &Keys, methods: &[SupportedPaymentMethod]) -> Result<Event> {
    let methods: Vec<serde_json::Value> = methods
        .iter()
        .map(|method| {
            serde_json::json!({
                "id": method.id,
                "kind": method.kind,
                "label": method.label,
                "currency": method.currency,
            })
        })
        .collect();
    let content = serde_json::json!({ "methods": methods }).to_string();
    Ok(EventBuilder::new(Kind::from(KIND_CATALOG), content)
        .tag(Tag::identifier(PAYMENT_CATALOG_TAG))
        .finalize(keys)?)
}

pub async fn publish_payment_catalog(
    client: &Client,
    keys: &Keys,
    methods: &[SupportedPaymentMethod],
) -> Result<()> {
    let event = payment_catalog_event(keys, methods)?;
    publish_event(client, &event).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fiber_node_event_is_signed_by_the_daemon() {
        let keys = Keys::generate();
        let event = fiber_node_event(&keys, " 02abc ", None).unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind, Kind::from(KIND_FIBER_NODE));
        assert!(event.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.len() >= 2 && values[0] == "d" && values[1] == FIBER_NODE_TAG
        }));
        let body: serde_json::Value = serde_json::from_str(event.content.as_str()).unwrap();
        assert_eq!(body["pubkey"], "02abc");
        assert!(body["solver"].is_null());
        let with_solver = fiber_node_event(&keys, "02abc", Some("solverpk")).unwrap();
        let solved: serde_json::Value = serde_json::from_str(with_solver.content.as_str()).unwrap();
        assert_eq!(solved["solver"], "solverpk");
        assert!(fiber_node_event(&keys, " ", None).is_err());
    }

    #[test]
    fn payment_catalog_event_lists_the_methods_the_daemon_accepts() {
        let keys = Keys::generate();
        let event = payment_catalog_event(
            &keys,
            &[SupportedPaymentMethod {
                id: "gtbank".into(),
                kind: "bank".into(),
                label: "GTBank".into(),
                currency: "NGN".into(),
            }],
        )
        .unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind, Kind::from(KIND_CATALOG));
        assert!(event.tags.iter().any(|tag| {
            let values = tag.as_slice();
            values.len() >= 2 && values[0] == "d" && values[1] == PAYMENT_CATALOG_TAG
        }));
        let body: serde_json::Value = serde_json::from_str(event.content.as_str()).unwrap();
        assert_eq!(body["methods"][0]["id"], "gtbank");
        assert_eq!(body["methods"][0]["currency"], "NGN");
    }
}
