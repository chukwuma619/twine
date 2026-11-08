mod invoice;
mod node;
mod payment;
mod watchtower;

pub use invoice::InvoiceRpc;
pub use node::{FiberNodeInfo, NodeRpc};
pub use payment::{
    PaymentRpc, payment_expiry_limited, payment_invoice_expired, payment_route_failed,
    route_not_ready,
};
pub use watchtower::WatchtowerRpc;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct InvoiceCreated {
    pub invoice: String,
    pub payment_hash: String,
}

#[derive(Debug, Clone)]
pub struct InvoiceInfo {
    pub invoice: String,
    pub payment_hash: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct PaymentInfo {
    pub payment_hash: String,
    pub status: String,
    pub preimage: Option<String>,
    pub failed_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashAlgorithm {
    #[default]
    CkbHash,
    Sha256,
}

impl HashAlgorithm {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CkbHash => "ckb_hash",
            Self::Sha256 => "sha256",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "ckb_hash" => Ok(Self::CkbHash),
            "sha256" => Ok(Self::Sha256),
            other => bail!("unsupported hash algorithm {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedInvoice {
    pub payment_hash: String,
    pub currency: String,
    pub amount: Option<u128>,
    pub hash_algorithm: HashAlgorithm,
    pub final_tlc_expiry_delta_ms: u64,
    pub expires_at_ms: Option<i64>,
}

pub trait FiberRpc: InvoiceRpc + WatchtowerRpc + PaymentRpc + NodeRpc {
    fn currency(&self) -> &str;
}

impl FiberRpc for HttpFiber {
    fn currency(&self) -> &str {
        &self.currency
    }
}

#[derive(Clone)]
pub struct HttpFiber {
    url: String,
    currency: String,
    auth: HeaderValue,
    http: reqwest::Client,
}

impl HttpFiber {
    pub fn new(
        url: impl Into<String>,
        currency: impl Into<String>,
        biscuit: impl AsRef<str>,
    ) -> Result<Self> {
        let token = biscuit.as_ref().trim();
        if token.is_empty() {
            bail!("TWINE_RPC_TOKEN is required");
        }
        let auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("TWINE_RPC_TOKEN is not a valid Authorization header value")?;
        Ok(Self {
            url: url.into(),
            currency: currency.into(),
            auth,
            http: reqwest::Client::new(),
        })
    }

    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let body = match params {
            Some(params) => json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": [params],
            }),
            None => json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": [],
            }),
        };
        let response: Value = self
            .http
            .post(&self.url)
            .header(AUTHORIZATION, self.auth.clone())
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(error) = response.get("error")
            && !error.is_null()
        {
            bail!("{method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("{method} returned no result"))
    }
}

/// `true` when the caller should store `current` as the pinned Fiber node.
/// Refuses when open holds belong to a different node, or to a store that was never pinned.
pub fn accept_node_pubkey(saved: Option<&str>, current: &str, open_holds: usize) -> Result<bool> {
    match saved {
        Some(saved) if saved == current => Ok(false),
        Some(saved) if open_holds > 0 => bail!(
            "Fiber node pubkey changed from {saved} to {current} while {open_holds} holds are open"
        ),
        Some(_) => Ok(true),
        None if open_holds > 0 => {
            bail!("{open_holds} holds are open but this store has no pinned Fiber pubkey")
        }
        None => Ok(true),
    }
}

fn payment_hash_from_invoice(invoice: &Value) -> Result<String> {
    invoice
        .pointer("/data/payment_hash")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("invoice is missing payment_hash"))
}

pub(crate) fn final_tlc_expiry_delta_ms(invoice: &Value) -> Result<u64> {
    let Some(attrs) = invoice.pointer("/data/attrs").and_then(Value::as_array) else {
        return Ok(crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS);
    };
    let mut found = None;
    for attr in attrs {
        let Some(value) = attr.get("final_htlc_minimum_expiry_delta") else {
            continue;
        };
        let delta = hex_u64_value(value).context("invoice final tlc expiry delta")?;
        if found.is_some_and(|previous| previous != delta) {
            bail!("invoice has conflicting final tlc expiry deltas");
        }
        found = Some(delta);
    }
    Ok(found.unwrap_or(crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS))
}

pub(crate) fn invoice_expires_at_ms(invoice: &Value) -> Result<Option<i64>> {
    let Some(secs) = expiry_time_secs(invoice)? else {
        return Ok(None);
    };
    let timestamp_ms = invoice_timestamp_ms(invoice)?;
    let extra_ms = i64::try_from(secs)
        .ok()
        .and_then(|secs| secs.checked_mul(1_000))
        .ok_or_else(|| anyhow!("invoice expiry is too large"))?;
    Ok(Some(timestamp_ms.saturating_add(extra_ms)))
}

fn expiry_time_secs(invoice: &Value) -> Result<Option<u64>> {
    let Some(attrs) = invoice.pointer("/data/attrs").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut found = None;
    for attr in attrs {
        let Some(value) = attr.get("expiry_time") else {
            continue;
        };
        let secs = hex_u64_value(value).context("invoice expiry")?;
        if found.is_some_and(|previous| previous != secs) {
            bail!("invoice has conflicting expiry times");
        }
        found = Some(secs);
    }
    Ok(found)
}

fn invoice_timestamp_ms(invoice: &Value) -> Result<i64> {
    let value = invoice
        .pointer("/data/timestamp")
        .ok_or_else(|| anyhow!("invoice expiry is missing a timestamp"))?;
    i64::try_from(hex_u64_value(value)?).context("invoice timestamp")
}

fn hex_u64_value(value: &Value) -> Result<u64> {
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("integer is not a hex string"))?;
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u64::from_str_radix(digits, 16).context("hex integer")
}

pub(crate) fn hash_algorithm_from_invoice(invoice: &Value) -> Result<HashAlgorithm> {
    let Some(attrs) = invoice.pointer("/data/attrs").and_then(Value::as_array) else {
        return Ok(HashAlgorithm::CkbHash);
    };
    let mut found = None;
    for attr in attrs {
        let Some(value) = attr.get("hash_algorithm") else {
            continue;
        };
        let name = value
            .as_str()
            .ok_or_else(|| anyhow!("invoice hash algorithm is not a string"))?;
        let algorithm = HashAlgorithm::parse(name)?;
        if found.is_some_and(|previous| previous != algorithm) {
            bail!("invoice has conflicting hash algorithms");
        }
        found = Some(algorithm);
    }
    Ok(found.unwrap_or(HashAlgorithm::CkbHash))
}

fn same_hash(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

pub(crate) fn hex_amount(value: Option<&Value>) -> Result<Option<u128>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("invoice amount is not a hex string"))?;
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if digits.is_empty() {
        bail!("invoice amount is empty");
    }
    let amount = u128::from_str_radix(digits, 16).context("invoice amount")?;
    Ok(Some(amount))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biscuit_header_is_a_bearer_token() {
        let fiber = HttpFiber::new("http://127.0.0.1:8229", "Fibt", "abc.def").unwrap();
        assert_eq!(fiber.auth, "Bearer abc.def");
        assert!(HttpFiber::new("http://127.0.0.1:8229", "Fibt", "  ").is_err());
    }

    #[test]
    fn a_changed_pubkey_is_refused_only_while_holds_are_open() {
        assert!(!accept_node_pubkey(Some("same"), "same", 2).unwrap());
        assert!(accept_node_pubkey(None, "node", 0).unwrap());
        assert!(accept_node_pubkey(Some("old"), "new", 0).unwrap());
        assert!(accept_node_pubkey(None, "node", 1).is_err());
        let error = accept_node_pubkey(Some("old"), "new", 3).unwrap_err();
        assert!(error.to_string().contains("old"));
        assert!(error.to_string().contains("new"));
    }

    #[test]
    fn reads_a_hex_invoice_amount() {
        let invoice = json!({
            "currency": "Fibt",
            "amount": "0x5f5e100",
            "data": { "payment_hash": "0xabc" }
        });
        assert_eq!(
            hex_amount(invoice.get("amount")).unwrap(),
            Some(100_000_000)
        );
        assert!(same_hash("0xAbC", "0xabc"));
    }

    #[test]
    fn a_missing_hash_algorithm_is_ckb_hash() {
        let omitted = json!({
            "currency": "Fibt",
            "data": {
                "payment_hash": "0xabc",
                "attrs": [{ "description": "buyer" }]
            }
        });
        assert_eq!(
            hash_algorithm_from_invoice(&omitted).unwrap(),
            HashAlgorithm::CkbHash
        );
        let explicit = json!({
            "data": {
                "payment_hash": "0xabc",
                "attrs": [{ "hash_algorithm": "sha256" }]
            }
        });
        assert_eq!(
            hash_algorithm_from_invoice(&explicit).unwrap(),
            HashAlgorithm::Sha256
        );
        let unknown = json!({
            "data": {
                "attrs": [{ "hash_algorithm": "blake2b" }]
            }
        });
        assert!(hash_algorithm_from_invoice(&unknown).is_err());
        let conflict = json!({
            "data": {
                "attrs": [
                    { "hash_algorithm": "ckb_hash" },
                    { "hash_algorithm": "sha256" }
                ]
            }
        });
        assert!(hash_algorithm_from_invoice(&conflict).is_err());
    }

    #[test]
    fn a_missing_final_delta_uses_fibers_one_day_default() {
        let omitted = json!({
            "data": { "attrs": [{ "description": "buyer" }] }
        });
        assert_eq!(
            final_tlc_expiry_delta_ms(&omitted).unwrap(),
            crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS
        );
        let set = json!({
            "data": {
                "attrs": [{ "final_htlc_minimum_expiry_delta": "0x36ee80" }]
            }
        });
        assert_eq!(final_tlc_expiry_delta_ms(&set).unwrap(), 3_600_000);
    }

    #[test]
    fn an_invoice_without_expiry_does_not_expire() {
        let open = json!({
            "data": {
                "timestamp": "0x5",
                "attrs": [{ "description": "buyer" }]
            }
        });
        assert_eq!(invoice_expires_at_ms(&open).unwrap(), None);
        let set = json!({
            "data": {
                "timestamp": "0x5",
                "attrs": [{ "expiry_time": "0xe10" }]
            }
        });
        assert_eq!(invoice_expires_at_ms(&set).unwrap(), Some(5 + 3_600_000));
        let missing_timestamp = json!({
            "data": { "attrs": [{ "expiry_time": "0xe10" }] }
        });
        assert!(invoice_expires_at_ms(&missing_timestamp).is_err());
        let conflict = json!({
            "data": {
                "timestamp": "0x5",
                "attrs": [
                    { "expiry_time": "0xe10" },
                    { "expiry_time": "0x1" }
                ]
            }
        });
        assert!(invoice_expires_at_ms(&conflict).is_err());
    }
}
