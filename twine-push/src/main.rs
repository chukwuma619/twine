mod auth;
mod fcm;
mod tokens;

use anyhow::{Context, Result};
use auth::{authorize, payload_hash};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
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
    public_url: Option<String>,
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
    let public_url = std::env::var("TWINE_PUSH_PUBLIC_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let app = App {
        tokens: Arc::new(Mutex::new(TokenStore::load(tokens::tokens_path_from_env()))),
        seen: Arc::new(Mutex::new(Seen::default())),
        fcm: Arc::new(fcm),
        public_url,
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

async fn register(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    match parse_authorized(&app, &headers, "/register", &body) {
        Ok((pubkey, token)) => {
            let token = token.trim();
            if token.is_empty() {
                return StatusCode::BAD_REQUEST;
            }
            app.tokens.lock().await.add(pubkey, token.to_string());
            StatusCode::NO_CONTENT
        }
        Err(status) => status,
    }
}

async fn unregister(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    match parse_authorized(&app, &headers, "/unregister", &body) {
        Ok((pubkey, token)) => {
            app.tokens.lock().await.remove(&pubkey, token.trim());
            StatusCode::NO_CONTENT
        }
        Err(status) => status,
    }
}

fn parse_authorized(
    app: &App,
    headers: &HeaderMap,
    path: &str,
    body: &Bytes,
) -> Result<(String, String), StatusCode> {
    let pubkey = authorize(headers, path, body, app.public_url.as_deref())
        .map_err(|deny| deny.status())?;
    let payload: Register =
        serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((pubkey, payload.token))
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
            public_url: None,
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
        use nostr::nips::nip98::{HttpData, HttpMethod};
        let app = test_app();
        let keys = Keys::generate();
        let body = serde_json::json!({ "token": "fcm-token" }).to_string();
        let auth = HttpData::new(
            Url::parse("http://127.0.0.1/register").unwrap(),
            HttpMethod::POST,
        )
        .payload(payload_hash(body.as_bytes()))
        .to_authorization(&keys)
        .await
        .unwrap();
        let response = router(app.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1")
                    .header("authorization", auth)
                    .body(Body::from(body))
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

    #[tokio::test]
    async fn register_without_auth_is_rejected() {
        let body = serde_json::json!({ "token": "fcm-token" }).to_string();
        let response = router(test_app())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn register_trims_and_rejects_blank_tokens() {
        use nostr::nips::nip98::{HttpData, HttpMethod};
        let app = test_app();
        let keys = Keys::generate();
        let blank = serde_json::json!({ "token": "   " }).to_string();
        let blank_auth = HttpData::new(
            Url::parse("http://127.0.0.1/register").unwrap(),
            HttpMethod::POST,
        )
        .payload(payload_hash(blank.as_bytes()))
        .to_authorization(&keys)
        .await
        .unwrap();
        let blank_response = router(app.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1")
                    .header("authorization", blank_auth)
                    .body(Body::from(blank))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blank_response.status(), StatusCode::BAD_REQUEST);

        let padded = serde_json::json!({ "token": "  fcm-token  " }).to_string();
        let padded_auth = HttpData::new(
            Url::parse("http://127.0.0.1/register").unwrap(),
            HttpMethod::POST,
        )
        .payload(payload_hash(padded.as_bytes()))
        .to_authorization(&keys)
        .await
        .unwrap();
        let padded_response = router(app.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("content-type", "application/json")
                    .header("host", "127.0.0.1")
                    .header("authorization", padded_auth)
                    .body(Body::from(padded))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(padded_response.status(), StatusCode::NO_CONTENT);
        assert!(
            app.tokens
                .lock()
                .await
                .tokens_for(&keys.public_key().to_hex())
                .any(|token| token == "fcm-token")
        );
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
