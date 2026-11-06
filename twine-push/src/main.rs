mod fcm;
mod tokens;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use fcm::{Fcm, FcmOutcome, ServiceAccount};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokens::TokenStore;
use tokio::sync::Mutex;
use tracing::{info, warn};

const KIND_ACTION: u16 = 4242;
const KIND_CHAT: u16 = 4243;
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RELAY_RETRY: Duration = Duration::from_secs(5);
const RECONNECT_GRACE: Duration = Duration::from_millis(250);
const SEEN_CAP: usize = 20_000;

#[derive(Clone)]
struct App {
    tokens: Arc<Mutex<TokenStore>>,
    seen: Arc<Mutex<Seen>>,
    fcm: Arc<Fcm>,
}

#[derive(Default)]
struct Seen {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Seen {
    fn insert(&mut self, id: String) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

#[derive(Deserialize)]
struct Register {
    pubkey: String,
    token: String,
}

#[derive(Serialize)]
struct OkBody {
    ok: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let relays = std::env::var("TWINE_RELAYS").unwrap_or_else(|_| "wss://relay.backr.site".into());
    let listen = std::env::var("TWINE_PUSH_LISTEN").unwrap_or_else(|_| "0.0.0.0:8787".into());
    let account = ServiceAccount::from_env()?;
    let http = reqwest::Client::new();
    let fcm = Fcm::new(account, http);
    if !fcm.configured() {
        warn!("FCM_SERVICE_ACCOUNT unset; doorbells will be logged only");
    }
    let app = App {
        tokens: Arc::new(Mutex::new(TokenStore::load(tokens::tokens_path_from_env()))),
        seen: Arc::new(Mutex::new(Seen::default())),
        fcm: Arc::new(fcm),
    };
    spawn_relay_watch(app.clone(), parse_relays(&relays));
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    info!(%listen, "twine-push listening");
    axum::serve(listener, router).await?;
    Ok(())
}

fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/register", post(register))
        .route("/unregister", post(unregister))
        .with_state(app)
}

async fn health() -> Json<OkBody> {
    Json(OkBody { ok: true })
}

async fn register(State(app): State<App>, Json(body): Json<Register>) -> StatusCode {
    let Some(pubkey) = normalize_pubkey(&body.pubkey) else {
        return StatusCode::BAD_REQUEST;
    };
    if body.token.trim().is_empty() {
        return StatusCode::BAD_REQUEST;
    }
    app.tokens
        .lock()
        .await
        .add(pubkey, body.token.trim().to_string());
    StatusCode::NO_CONTENT
}

async fn unregister(State(app): State<App>, Json(body): Json<Register>) -> StatusCode {
    let Some(pubkey) = normalize_pubkey(&body.pubkey) else {
        return StatusCode::BAD_REQUEST;
    };
    app.tokens
        .lock()
        .await
        .remove(&pubkey, body.token.trim());
    StatusCode::NO_CONTENT
}

fn normalize_pubkey(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.to_ascii_lowercase().starts_with("nsec1") {
        return None;
    }
    PublicKey::parse(trimmed).ok().map(|key| key.to_hex())
}

fn parse_relays(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn watch_filter() -> Filter {
    Filter::new()
        .kind(Kind::from(KIND_ACTION))
        .kind(Kind::from(KIND_CHAT))
        .limit(0)
}

fn spawn_relay_watch(app: App, relays: Vec<String>) {
    tokio::spawn(async move {
        if let Err(error) = watch_relays(app, relays).await {
            warn!(%error, "relay watch ended");
        }
    });
}

async fn watch_relays(app: App, relays: Vec<String>) -> Result<()> {
    let client = Client::builder().build();
    for relay in &relays {
        client
            .add_relay(relay)
            .retry_interval(RELAY_RETRY)
            .adjust_retry_interval(false)
            .await?;
    }
    loop {
        wake_disconnected(&client).await;
        client.connect().and_wait(RELAY_CONNECT_TIMEOUT).await;
        if let Err(error) = client.subscribe(watch_filter()).await {
            warn!(%error, "push subscribe failed");
        }
        let mut notifications = client.notifications();
        while let Some(notification) = notifications.next().await {
            match notification {
                ClientNotification::Event { event, .. } => {
                    if let Err(error) = notify_event(&app, &event).await {
                        warn!(%error, "notify failed");
                    }
                }
                ClientNotification::Message { message, .. } => {
                    if matches!(message.as_ref(), RelayMessage::Closed { .. }) {
                        warn!("push subscription closed; re-issuing");
                        break;
                    }
                }
                ClientNotification::Shutdown => {
                    warn!("push notification stream ended; reopening");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wake_disconnected(client: &Client) {
    let mut urls = Vec::new();
    for (url, relay) in client.relays().await {
        if relay.status() == RelayStatus::Disconnected {
            urls.push(url.to_string());
        }
    }
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
}

async fn notify_event(app: &App, event: &Event) -> Result<()> {
    {
        let mut seen = app.seen.lock().await;
        if !seen.insert(event.id.to_hex()) {
            return Ok(());
        }
    }
    let kind = event.kind.as_u16();
    let wake = if kind == KIND_CHAT {
        "chat_wake"
    } else {
        "trade_update"
    };
    let recipients: Vec<String> = event
        .tags
        .public_keys()
        .map(|key| key.to_hex())
        .collect();
    let mut deliver = Vec::new();
    {
        let tokens = app.tokens.lock().await;
        for pubkey in &recipients {
            deliver.extend(tokens.tokens_for(pubkey).cloned());
        }
    }
    for token in deliver {
        match app.fcm.send(&token, wake).await {
            FcmOutcome::Rejected => app.tokens.lock().await.drop_token(&token),
            FcmOutcome::Sent | FcmOutcome::Skipped | FcmOutcome::Failed => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> App {
        App {
            tokens: Arc::new(Mutex::new(TokenStore::memory())),
            seen: Arc::new(Mutex::new(Seen::default())),
            fcm: Arc::new(Fcm::new(None, reqwest::Client::new())),
        }
    }

    #[tokio::test]
    async fn health_is_ok() {
        let response = router(test_app())
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), br#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn register_stores_a_token() {
        let app = test_app();
        let keys = Keys::generate();
        let body = serde_json::json!({
            "pubkey": keys.public_key().to_hex(),
            "token": "fcm-token",
        });
        let response = router(app.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            app.tokens
                .lock()
                .await
                .tokens_for(&keys.public_key().to_hex())
                .any(|token| token == "fcm-token")
        );
    }

    #[test]
    fn nsec_is_not_a_registration_key() {
        assert!(normalize_pubkey("nsec1qqqq").is_none());
        assert!(normalize_pubkey("not-a-key").is_none());
    }

    #[test]
    fn watch_is_live_only() {
        let filter = watch_filter();
        assert_eq!(filter.limit, Some(0));
        let kinds = filter.kinds.expect("kinds");
        assert!(kinds.contains(&Kind::from(KIND_ACTION)));
        assert!(kinds.contains(&Kind::from(KIND_CHAT)));
    }

    #[test]
    fn seen_evicts_the_oldest_id() {
        let mut seen = Seen::default();
        assert!(seen.insert("a".into()));
        assert!(!seen.insert("a".into()));
        for i in 0..=SEEN_CAP {
            seen.insert(format!("id-{i}"));
        }
        assert!(seen.insert("a".into()));
    }
}
