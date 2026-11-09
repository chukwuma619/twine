use axum::http::HeaderMap;
use nostr::nips::nip98::{HttpMethod, verify_auth_header};
use nostr::prelude::*;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusDeny {
    BadRequest,
    Unauthorized,
}

impl StatusDeny {
    pub fn status(self) -> axum::http::StatusCode {
        match self {
            Self::BadRequest => axum::http::StatusCode::BAD_REQUEST,
            Self::Unauthorized => axum::http::StatusCode::UNAUTHORIZED,
        }
    }
}

pub fn authorize(
    headers: &HeaderMap,
    path: &str,
    body: &[u8],
    public_url: Option<&str>,
) -> Result<String, StatusDeny> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusDeny::Unauthorized)?;
    let url = request_url(headers, path, public_url).ok_or(StatusDeny::BadRequest)?;
    let pubkey = verify_auth_header(
        auth,
        &url,
        HttpMethod::POST,
        Timestamp::now(),
        Some(body),
    )
    .map_err(|_| StatusDeny::Unauthorized)?;
    Ok(pubkey.to_hex())
}

pub fn payload_hash(body: &[u8]) -> Sha256Hash {
    Sha256Hash::from_byte_array(Sha256::digest(body).into())
}

fn request_url(headers: &HeaderMap, path: &str, public_url: Option<&str>) -> Option<Url> {
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if let Some(base) = public_url.map(str::trim).filter(|base| !base.is_empty()) {
        let base = base.trim_end_matches('/');
        return Url::parse(&format!("{base}{path}")).ok();
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())?
        .trim();
    if host.is_empty() {
        return None;
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| {
            if host.starts_with("127.") || host.starts_with("localhost") {
                "http"
            } else {
                "https"
            }
        });
    Url::parse(&format!("{proto}://{host}{path}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use nostr::nips::nip98::{HttpData, HttpMethod};

    async fn signed_auth(keys: &Keys, url: &str, body: &[u8]) -> String {
        HttpData::new(Url::parse(url).unwrap(), HttpMethod::POST)
            .payload(payload_hash(body))
            .to_authorization(keys)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn valid_auth_returns_the_pubkey() {
        let keys = Keys::generate();
        let body = br#"{"token":"fcm"}"#;
        let url = "http://127.0.0.1:8787/register";
        let auth = signed_auth(&keys, url, body).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&auth).unwrap(),
        );
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8787"),
        );
        let pubkey = authorize(&headers, "/register", body, None).unwrap();
        assert_eq!(pubkey, keys.public_key().to_hex());
    }

    #[tokio::test]
    async fn wrong_body_is_rejected() {
        let keys = Keys::generate();
        let body = br#"{"token":"fcm"}"#;
        let url = "http://127.0.0.1:8787/register";
        let auth = signed_auth(&keys, url, body).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&auth).unwrap(),
        );
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8787"),
        );
        assert_eq!(
            authorize(&headers, "/register", br#"{"token":"other"}"#, None),
            Err(StatusDeny::Unauthorized)
        );
    }

    #[tokio::test]
    async fn public_url_overrides_host() {
        let keys = Keys::generate();
        let body = br#"{"token":"fcm"}"#;
        let url = "https://push.example/register";
        let auth = signed_auth(&keys, url, body).await;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&auth).unwrap(),
        );
        headers.insert(
            axum::http::header::HOST,
            HeaderValue::from_static("127.0.0.1:8787"),
        );
        let pubkey = authorize(&headers, "/register", body, Some("https://push.example")).unwrap();
        assert_eq!(pubkey, keys.public_key().to_hex());
    }
}
