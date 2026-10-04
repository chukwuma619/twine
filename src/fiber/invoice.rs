use super::{HttpFiber, InvoiceCreated, InvoiceInfo, ParsedInvoice};
use crate::{FINAL_EXPIRY_DELTA_MS, HASH_ALGORITHM, INVOICE_EXPIRY_SECS, hex_u64, hex_u128};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait InvoiceRpc: Send + Sync {
    async fn new_hold_invoice(
        &self,
        amount: u128,
        payment_hash: &str,
        description: &str,
        final_expiry_delta_ms: u64,
    ) -> Result<InvoiceCreated>;
    async fn new_payout_invoice(&self, amount: u128, description: &str) -> Result<InvoiceCreated>;
    async fn get_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo>;
    async fn cancel_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo>;
    async fn settle_invoice(&self, payment_hash: &str, preimage: &str) -> Result<()>;
    async fn parse_invoice(&self, invoice: &str) -> Result<ParsedInvoice>;
}

#[async_trait]
impl InvoiceRpc for HttpFiber {
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
        let got_hash = super::payment_hash_from_invoice(invoice)?;
        if !super::same_hash(&got_hash, payment_hash) {
            bail!("hold invoice hash {got_hash} does not match {payment_hash}");
        }
        match super::hex_amount(invoice.get("amount"))? {
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
            payment_hash: super::payment_hash_from_invoice(&result["invoice"])?,
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
            payment_hash: super::payment_hash_from_invoice(&result["invoice"])?,
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

    async fn parse_invoice(&self, invoice: &str) -> Result<ParsedInvoice> {
        let result = self
            .call("parse_invoice", Some(json!({ "invoice": invoice })))
            .await?;
        let invoice = &result["invoice"];
        Ok(ParsedInvoice {
            payment_hash: super::payment_hash_from_invoice(invoice)?,
            currency: invoice
                .get("currency")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            amount: super::hex_amount(invoice.get("amount"))?,
        })
    }
}
