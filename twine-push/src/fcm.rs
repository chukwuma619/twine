use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{info, warn};

const MESSAGING_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const TOKEN_AUD: &str = "https://oauth2.googleapis.com/token";
const TOKEN_TTL: Duration = Duration::from_secs(3500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FcmOutcome {
    Sent,
    Skipped,
    Failed,
    Rejected,
}

pub fn classify_response(status: u16, body: &str) -> FcmOutcome {
    if (200..300).contains(&status) {
        return FcmOutcome::Sent;
    }
    if status == 404 || body.contains("UNREGISTERED") || body.contains("NOT_FOUND") {
        return FcmOutcome::Rejected;
    }
    FcmOutcome::Failed
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    TOKEN_AUD.to_string()
}

impl ServiceAccount {
    pub fn from_json(raw: &str) -> Result<Self> {
        let account: Self = serde_json::from_str(raw).context("invalid FCM service account JSON")?;
        if account.project_id.is_empty()
            || account.client_email.is_empty()
            || account.private_key.is_empty()
        {
            bail!("FCM service account is missing project_id, client_email, or private_key");
        }
        Ok(account)
    }

    pub fn from_env() -> Result<Option<Self>> {
        if let Ok(raw) = std::env::var("FCM_SERVICE_ACCOUNT") {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            if Path::new(trimmed).is_file() {
                return Ok(Some(Self::from_json(&std::fs::read_to_string(trimmed)?)?));
            }
            return Ok(Some(Self::from_json(trimmed)?));
        }
        if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Self::from_json(&std::fs::read_to_string(trimmed)?)?));
        }
        Ok(None)
    }
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

struct CachedToken {
    value: String,
    expires_at: SystemTime,
}

pub struct Fcm {
    account: Option<ServiceAccount>,
    http: reqwest::Client,
    access: Mutex<Option<CachedToken>>,
}

impl Fcm {
    pub fn new(account: Option<ServiceAccount>, http: reqwest::Client) -> Self {
        Self {
            account,
            http,
            access: Mutex::new(None),
        }
    }

    pub fn configured(&self) -> bool {
        self.account.is_some()
    }

    pub fn message_body(fid: &str, wake: &str) -> serde_json::Value {
        serde_json::json!({
            "message": {
                "fid": fid,
                "data": { "type": wake },
                "android": { "priority": "HIGH" },
                "apns": {
                    "headers": {
                        "apns-priority": "10",
                        "apns-push-type": "background"
                    },
                    "payload": {
                        "aps": { "content-available": 1 }
                    }
                }
            }
        })
    }

    pub async fn send(&self, fid: &str, wake: &str) -> FcmOutcome {
        let Some(account) = &self.account else {
            info!(%fid, %wake, "skip fcm; FCM_SERVICE_ACCOUNT unset");
            return FcmOutcome::Skipped;
        };
        let access = match self.access_token(account).await {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, "fcm auth failed");
                return FcmOutcome::Failed;
            }
        };
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            account.project_id
        );
        let response = match self
            .http
            .post(url)
            .bearer_auth(access)
            .json(&Self::message_body(fid, wake))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                warn!(%error, "fcm request failed");
                return FcmOutcome::Failed;
            }
        };
        if response.status().is_success() {
            return FcmOutcome::Sent;
        }
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        let outcome = classify_response(status, &body);
        warn!(%status, %body, ?outcome, "fcm rejected");
        outcome
    }

    async fn access_token(&self, account: &ServiceAccount) -> Result<String> {
        {
            let cached = self.access.lock().await;
            if let Some(token) = cached.as_ref() {
                if token.expires_at > SystemTime::now() {
                    return Ok(token.value.clone());
                }
            }
        }
        let assertion = assertion_jwt(account)?;
        let response = self
            .http
            .post(&account.token_uri)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(format!(
                "grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion={assertion}"
            ))
            .send()
            .await
            .context("FCM OAuth token request failed")?;
        if !response.status().is_success() {
            bail!("FCM OAuth token returned {}", response.status());
        }
        let TokenResponse { access_token } = response.json().await?;
        *self.access.lock().await = Some(CachedToken {
            value: access_token.clone(),
            expires_at: SystemTime::now() + TOKEN_TTL,
        });
        Ok(access_token)
    }
}

pub fn assertion_jwt(account: &ServiceAccount) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = JwtClaims {
        iss: &account.client_email,
        scope: MESSAGING_SCOPE,
        aud: TOKEN_AUD,
        iat: now,
        exp: now + 3600,
    };
    encode(
        &Header::new(Algorithm::RS256),
        &claims,
        &EncodingKey::from_rsa_pem(account.private_key.as_bytes())
            .context("FCM service account private_key is not a PEM RSA key")?,
    )
    .context("sign FCM JWT")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_account_requires_the_fcm_fields() {
        let parsed = ServiceAccount::from_json(
            r#"{
                "project_id": "twine",
                "client_email": "push@twine.iam.gserviceaccount.com",
                "private_key": "-----BEGIN PRIVATE KEY-----\nX\n-----END PRIVATE KEY-----\n"
            }"#,
        )
        .unwrap();
        assert_eq!(parsed.project_id, "twine");
        assert_eq!(parsed.token_uri, TOKEN_AUD);
        assert!(ServiceAccount::from_json(r#"{"project_id":"twine"}"#).is_err());
    }

    #[test]
    fn doorbell_targets_firebase_installation_id() {
        let body = Fcm::message_body("device-fid", "chat_wake");
        let message = &body["message"];
        assert_eq!(message["fid"], "device-fid");
        assert!(message.get("token").is_none());
        assert_eq!(message["data"]["type"], "chat_wake");
        assert!(message.get("notification").is_none());
        assert_eq!(message["android"]["priority"], "HIGH");
        assert_eq!(message["apns"]["payload"]["aps"]["content-available"], 1);
    }

    #[test]
    fn unregistered_tokens_are_dropped() {
        assert_eq!(classify_response(404, ""), FcmOutcome::Rejected);
        assert_eq!(
            classify_response(400, r#"{"error":{"status":"UNREGISTERED"}}"#),
            FcmOutcome::Rejected
        );
        assert_eq!(classify_response(500, "unavailable"), FcmOutcome::Failed);
        assert_eq!(classify_response(200, ""), FcmOutcome::Sent);
    }
}
