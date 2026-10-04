use crate::{
    FINAL_EXPIRY_DELTA_MS, HASH_ALGORITHM, INVOICE_EXPIRY_SECS, MAX_FEE_RATE, hex_u64, hex_u128,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InvoiceCreated {
    pub invoice: String,
    pub payment_hash: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InvoiceInfo {
    pub invoice: String,
    pub payment_hash: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct PaymentInfo {
    pub payment_hash: String,
    pub status: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedInvoice {
    pub payment_hash: String,
    pub currency: String,
    pub amount: Option<u128>,
}

#[async_trait]
pub trait FiberRpc: Send + Sync {
    async fn new_hold_invoice(
        &self,
        amount: u128,
        payment_hash: &str,
        description: &str,
        final_expiry_delta_ms: u64,
    ) -> Result<InvoiceCreated>;
    #[allow(dead_code)]
    async fn new_payout_invoice(&self, amount: u128, description: &str) -> Result<InvoiceCreated>;
    async fn get_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo>;
    async fn cancel_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo>;
    async fn settle_invoice(&self, payment_hash: &str, preimage: &str) -> Result<()>;
    async fn create_preimage(&self, payment_hash: &str, preimage: &str) -> Result<()>;
    async fn remove_preimage(&self, payment_hash: &str) -> Result<()>;
    async fn send_payment(&self, invoice: &str) -> Result<PaymentInfo>;
    async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo>;
    async fn parse_invoice(&self, invoice: &str) -> Result<ParsedInvoice>;
    async fn node_pubkey(&self) -> Result<String>;
    fn currency(&self) -> &str;
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
        if let Some(error) = response.get("error") {
            if !error.is_null() {
                bail!("{method} failed: {error}");
            }
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

#[async_trait]
impl FiberRpc for HttpFiber {
    async fn new_hold_invoice(
        &self,
        amount: u128,
        payment_hash: &str,
        description: &str,
        final_expiry_delta_ms: u64,
    ) -> Result<InvoiceCreated> {
        let result = self
            .call(
                "new_invoice",
                Some(json!({
                    "amount": hex_u128(amount),
                    "currency": self.currency,
                    "payment_hash": payment_hash,
                    "hash_algorithm": HASH_ALGORITHM,
                    "final_expiry_delta": hex_u64(final_expiry_delta_ms),
                    "expiry": hex_u64(INVOICE_EXPIRY_SECS),
                    "description": description,
                })),
            )
            .await?;
        let invoice = &result["invoice"];
        let got_hash = payment_hash_from_invoice(invoice)?;
        if !same_hash(&got_hash, payment_hash) {
            bail!("hold invoice hash {got_hash} does not match {payment_hash}");
        }
        match hex_amount(invoice.get("amount"))? {
            Some(got) if got == amount => {}
            Some(got) => bail!("hold invoice amount {got} does not match {amount}"),
            None => bail!("hold invoice is missing an amount"),
        }
        Ok(InvoiceCreated {
            invoice: result["invoice_address"]
                .as_str()
                .ok_or_else(|| anyhow!("new_invoice missing invoice_address"))?
                .to_string(),
            payment_hash: got_hash,
        })
    }

    async fn new_payout_invoice(&self, amount: u128, description: &str) -> Result<InvoiceCreated> {
        let result = self
            .call(
                "new_invoice",
                Some(json!({
                    "amount": hex_u128(amount),
                    "currency": self.currency,
                    "final_expiry_delta": hex_u64(FINAL_EXPIRY_DELTA_MS),
                    "description": description,
                })),
            )
            .await?;
        Ok(InvoiceCreated {
            invoice: result["invoice_address"]
                .as_str()
                .ok_or_else(|| anyhow!("new_invoice missing invoice_address"))?
                .to_string(),
            payment_hash: payment_hash_from_invoice(&result["invoice"])?,
        })
    }

    async fn get_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo> {
        let result = self
            .call("get_invoice", Some(json!({ "payment_hash": payment_hash })))
            .await?;
        Ok(InvoiceInfo {
            invoice: result["invoice_address"]
                .as_str()
                .unwrap_or_default()
                .into(),
            payment_hash: payment_hash_from_invoice(&result["invoice"])?,
            status: result["status"]
                .as_str()
                .ok_or_else(|| anyhow!("get_invoice missing status"))?
                .to_string(),
        })
    }

    async fn cancel_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo> {
        let result = self
            .call(
                "cancel_invoice",
                Some(json!({ "payment_hash": payment_hash })),
            )
            .await?;
        Ok(InvoiceInfo {
            invoice: result["invoice_address"]
                .as_str()
                .unwrap_or_default()
                .into(),
            payment_hash: payment_hash.to_string(),
            status: result["status"].as_str().unwrap_or("Cancelled").to_string(),
        })
    }

    async fn settle_invoice(&self, payment_hash: &str, preimage: &str) -> Result<()> {
        self.call(
            "settle_invoice",
            Some(json!({
                "payment_hash": payment_hash,
                "payment_preimage": preimage,
            })),
        )
        .await?;
        Ok(())
    }

    async fn create_preimage(&self, payment_hash: &str, preimage: &str) -> Result<()> {
        self.call(
            "create_preimage",
            Some(json!({
                "payment_hash": payment_hash,
                "preimage": preimage,
            })),
        )
        .await?;
        Ok(())
    }

    async fn remove_preimage(&self, payment_hash: &str) -> Result<()> {
        self.call(
            "remove_preimage",
            Some(json!({ "payment_hash": payment_hash })),
        )
        .await?;
        Ok(())
    }

    async fn send_payment(&self, invoice: &str) -> Result<PaymentInfo> {
        let result = self
            .call(
                "send_payment",
                Some(json!({
                    "invoice": invoice,
                    "max_fee_rate": hex_u64(MAX_FEE_RATE),
                })),
            )
            .await?;
        Ok(PaymentInfo {
            payment_hash: result["payment_hash"]
                .as_str()
                .ok_or_else(|| anyhow!("send_payment missing payment_hash"))?
                .to_string(),
            status: result["status"].as_str().unwrap_or("Created").to_string(),
        })
    }

    async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo> {
        let result = self
            .call("get_payment", Some(json!({ "payment_hash": payment_hash })))
            .await?;
        Ok(PaymentInfo {
            payment_hash: payment_hash.to_string(),
            status: result["status"]
                .as_str()
                .ok_or_else(|| anyhow!("get_payment missing status"))?
                .to_string(),
        })
    }

    async fn parse_invoice(&self, invoice: &str) -> Result<ParsedInvoice> {
        let result = self
            .call("parse_invoice", Some(json!({ "invoice": invoice })))
            .await?;
        let invoice = &result["invoice"];
        Ok(ParsedInvoice {
            payment_hash: payment_hash_from_invoice(invoice)?,
            currency: invoice
                .get("currency")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            amount: hex_amount(invoice.get("amount"))?,
        })
    }

    async fn node_pubkey(&self) -> Result<String> {
        let result = self.call("node_info", None).await?;
        result["pubkey"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("node_info missing pubkey"))
    }

    fn currency(&self) -> &str {
        &self.currency
    }
}

fn same_hash(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn hex_amount(value: Option<&Value>) -> Result<Option<u128>> {
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
        let fiber = HttpFiber::new("http://127.0.0.1:8227", "Fibt", "abc.def").unwrap();
        assert_eq!(fiber.auth, "Bearer abc.def");
        assert!(HttpFiber::new("http://127.0.0.1:8227", "Fibt", "  ").is_err());
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
}
