use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;
use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};
use twine_daemon::db::Db;
use twine_daemon::engine::Engine;
use twine_daemon::fiber::{FiberNodeInfo, HttpFiber, NodeRpc, accept_node_pubkey};
use twine_daemon::nostr::{
    connect, connected_relay_urls, decrypt_action, ensure_action_subscription, public_order_event,
    publish_event, publish_fiber_node, publish_outbounds, sender_hex,
};
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
    let npub = keys.public_key().to_bech32()?;
    info!(
        %npub,
        relays = %config.relays.join(","),
        "daemon address for the app"
    );

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
    let client = connect(&keys, &config.relays).await?;

    let polling = Arc::new(Mutex::new(HashSet::new()));
    engine.bind_polls(client.clone(), keys.clone(), polling);
    let (holds, payouts) = engine.resume_trade_ids()?;
    for trade_id in holds.into_iter().chain(payouts) {
        engine.watch(&trade_id)?;
    }

    let mut notifications = client.notifications();
    if let Err(error) = ensure_action_subscription(&client, keys.public_key()).await {
        warn!(%error, "action subscription failed; will retry");
    }
    spawn_relay_maintenance(
        client.clone(),
        keys.clone(),
        engine.db.clone(),
        config.solver.clone(),
    );

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

const RELAY_MAINTENANCE: Duration = Duration::from_secs(5);

fn spawn_relay_maintenance(client: Client, keys: Keys, db: Arc<Db>, solver: Option<String>) {
    tokio::spawn(async move {
        let mut published_to = BTreeSet::new();
        let mut reported_down = false;
        loop {
            client.connect().await;
            let up = connected_relay_urls(&client).await;
            if up.is_empty() {
                published_to.clear();
                if !reported_down {
                    warn!("no nostr relay connected; keeping the retry loop");
                    reported_down = true;
                }
            } else if reported_down {
                info!(relays = up.len(), "nostr relays connected again");
                reported_down = false;
            }
            if let Err(error) = ensure_action_subscription(&client, keys.public_key()).await {
                warn!(%error, "action subscription failed; will retry");
            }
            if !up.is_empty() && !up.is_subset(&published_to) {
                match publish_presence(&client, &keys, &db, solver.as_deref()).await {
                    Ok(()) => published_to.extend(up),
                    Err(error) => {
                        warn!(%error, "republish after connect failed; will retry");
                        published_to.clear();
                    }
                }
            }
            tokio::time::sleep(RELAY_MAINTENANCE).await;
        }
    });
}

async fn publish_presence(
    client: &Client,
    keys: &Keys,
    db: &Db,
    solver: Option<&str>,
) -> Result<()> {
    let mut failed = false;
    match db.fiber_pubkey() {
        Ok(Some(pubkey)) => {
            let addresses = db.fiber_addresses().unwrap_or_default();
            if addresses.is_empty() {
                warn!(
                    pubkey = %pubkey,
                    "fiber node has no public multiaddr; Fiber Studio cannot dial it. Set announced_addrs"
                );
            } else {
                info!(pubkey = %pubkey, addresses = ?addresses, "fiber node for the app");
            }
            if let Err(error) = publish_fiber_node(client, keys, &pubkey, solver, &addresses).await
            {
                warn!(%error, "fiber node announcement failed");
                failed = true;
            }
        }
        Ok(None) => warn!("fiber node is not pinned"),
        Err(error) => {
            warn!(%error, "fiber node lookup failed");
            failed = true;
        }
    }
    match db.orders() {
        Ok(orders) => {
            for order in orders {
                let order_id = order.id.clone();
                let event = match public_order_event(keys, &order.public()) {
                    Ok(event) => event,
                    Err(error) => {
                        warn!(%error, %order_id, "order event failed");
                        continue;
                    }
                };
                if let Err(error) = publish_event(client, &event).await {
                    warn!(%error, %order_id, "order never reached a relay");
                    failed = true;
                }
            }
        }
        Err(error) => {
            warn!(%error, "could not load orders to republish");
            failed = true;
        }
    }
    if failed {
        bail!("republish failed");
    }
    Ok(())
}

async fn pin_fiber_node(db: &Db, fiber: &HttpFiber) -> Result<FiberNodeInfo> {
    let info = fiber
        .node_info()
        .await
        .context("Fiber RPC node_info failed")?;
    let open = db.trades_in_states(Phase::watched_phases())?.len();
    let saved = db.fiber_pubkey()?;
    if accept_node_pubkey(saved.as_deref(), &info.pubkey, open)? {
        db.set_fiber_node(&info.pubkey, &info.addresses)?;
        info!(pubkey = %info.pubkey, addresses = ?info.addresses, "pinned fiber node");
    } else {
        // Already this pubkey. Refresh multiaddrs without replacing the pin.
        db.update_pinned_addresses(&info.pubkey, &info.addresses)?;
        info!(pubkey = %info.pubkey, addresses = ?info.addresses, open_holds = open, "fiber node");
    }
    Ok(info)
}
