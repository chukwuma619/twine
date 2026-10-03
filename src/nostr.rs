use crate::types::Outbound;
use crate::{Envelope, KIND_ACTION, KIND_ORDER};
use anyhow::{Context, Result};
use nostr::nips::nip44::{self, Version};
use nostr_sdk::prelude::*;

pub async fn connect(relays: &[String]) -> Result<Client> {
    let client = Client::default();
    for relay in relays {
        client
            .add_relay(relay)
            .await
            .with_context(|| relay.clone())?;
    }
    client.connect().await;
    Ok(client)
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
                client.send_event(&event).await?;
            }
            Outbound::PublicOrder(order) => {
                let event =
                    EventBuilder::new(Kind::from(KIND_ORDER), serde_json::to_string(order)?)
                        .tag(Tag::identifier(&order.order_id))
                        .finalize(keys)?;
                client.send_event(&event).await?;
            }
        }
    }
    Ok(())
}

pub fn sender_hex(event: &Event) -> String {
    event.pubkey.to_hex()
}
