use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use twine_daemon::db::Db;
use twine_daemon::engine::Engine;
use twine_daemon::fiber::{FiberRpc, HttpFiber, accept_node_pubkey};
use twine_daemon::nostr::{action_filter, connect, decrypt_action, publish_outbounds, sender_hex};
use twine_daemon::{Config, KIND_ACTION, Phase};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let keys = Keys::parse(&config.nostr_secret)?;
    info!(pubkey = %keys.public_key(), "twine daemon");

    let db = Arc::new(Db::open(&config.db_path)?);
    info!(network = %config.network, currency = %config.invoice_currency, "fiber");
    let fiber = Arc::new(HttpFiber::new(
        &config.rpc_url,
        &config.invoice_currency,
        &config.rpc_token,
    )?);
    pin_fiber_node(&db, fiber.as_ref()).await?;
    let engine = Engine::new(db, fiber, config.solver.clone());
    engine.arm_watchtower().await?;
    let client = connect(&config.relays).await?;

    let polling = Arc::new(Mutex::new(HashSet::new()));
    engine.bind_polls(client.clone(), keys.clone(), polling);
    let (holds, payouts) = engine.resume_trade_ids()?;
    for trade_id in holds.into_iter().chain(payouts) {
        engine.watch(&trade_id)?;
    }

    client.subscribe(action_filter(keys.public_key())).await?;
    let mut notifications = client.notifications();

    while let Some(notification) = notifications.next().await {
        let ClientNotification::Event { event, .. } = notification else {
            continue;
        };
        if event.kind != Kind::from(KIND_ACTION) {
            continue;
        }
        if !event
            .tags
            .public_keys()
            .any(|pubkey| pubkey == keys.public_key())
        {
            continue;
        }
        if !engine.db.mark_event(&event.id.to_hex())? {
            continue;
        }
        let envelope = match decrypt_action(&keys, &event) {
            Ok(envelope) => envelope,
            Err(error) => {
                warn!(%error, "rejecting event");
                continue;
            }
        };
        let sender = sender_hex(&event);
        match engine.handle(&sender, envelope).await {
            Ok(outbound) => {
                if let Err(error) = publish_outbounds(&client, &keys, &outbound).await {
                    warn!(%error, "publish failed");
                }
            }
            Err(error) => warn!(%error, "handle failed"),
        }
    }
    Ok(())
}

async fn pin_fiber_node(db: &Db, fiber: &HttpFiber) -> Result<()> {
    let current = fiber
        .node_pubkey()
        .await
        .context("Fiber RPC node_info failed")?;
    let open = db.trades_in_states(Phase::watched_phases())?.len();
    let saved = db.fiber_pubkey()?;
    if accept_node_pubkey(saved.as_deref(), &current, open)? {
        db.set_fiber_pubkey(&current)?;
        info!(pubkey = %current, "pinned fiber node");
    } else {
        info!(pubkey = %current, open_holds = open, "fiber node");
    }
    Ok(())
}
