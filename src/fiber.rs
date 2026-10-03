use crate::{
    FINAL_EXPIRY_DELTA_MS, HASH_ALGORITHM, INVOICE_EXPIRY_SECS, MAX_FEE_RATE, hex_u64, hex_u128,
};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
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
    pub amount: Option<String>,
}

#[async_trait]
pub trait FiberRpc: Send + Sync {
    async fn new_hold_invoice(
        &self,
        amount: u128,
        payment_hash: &str,
        description: &str,
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
    #[allow(dead_code)]
    async fn node_pubkey(&self) -> Result<String>;
}

#[derive(Clone)]
pub struct HttpFiber {
    url: String,
    currency: String,
    http: reqwest::Client,
}

impl HttpFiber {
    pub fn new(url: impl Into<String>, currency: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            currency: currency.into(),
            http: reqwest::Client::new(),
        }
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
    ) -> Result<InvoiceCreated> {
        let result = self
            .call(
                "new_invoice",
                Some(json!({
                    "amount": hex_u128(amount),
                    "currency": self.currency,
                    "payment_hash": payment_hash,
                    "hash_algorithm": HASH_ALGORITHM,
                    "final_expiry_delta": hex_u64(FINAL_EXPIRY_DELTA_MS),
                    "expiry": hex_u64(INVOICE_EXPIRY_SECS),
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
        Ok(ParsedInvoice {
            payment_hash: payment_hash_from_invoice(&result["invoice"])?,
            amount: result["invoice"]["amount"].as_str().map(ToOwned::to_owned),
        })
    }

    async fn node_pubkey(&self) -> Result<String> {
        let result = self.call("node_info", None).await?;
        result["pubkey"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("node_info missing pubkey"))
    }
}
