use super::{HashAlgorithm, HttpFiber, InvoiceCreated, InvoiceInfo, ParsedInvoice};
use crate::{hex_u64, hex_u128};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait InvoiceRpc: Send + Sync {
    async fn new_hold_invoice(
        &self,
        amount: u128,
        payment_hash: &str,
        hash_algorithm: HashAlgorithm,
        description: &str,
        final_expiry_delta_ms: u64,
        expiry_secs: u64,
    ) -> Result<InvoiceCreated>;
    async fn new_payout_invoice(
        &self,
        amount: u128,
        description: &str,
        final_expiry_delta_ms: u64,
    ) -> Result<InvoiceCreated>;
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
        hash_algorithm: HashAlgorithm,
        description: &str,
        final_expiry_delta_ms: u64,
        expiry_secs: u64,
    ) -> Result<InvoiceCreated> {
        let result = self
            .call(
                "new_invoice",
                Some(json!({
                    "amount": hex_u128(amount),
                    "currency": self.currency,
                    "payment_hash": payment_hash,
                    "hash_algorithm": hash_algorithm.as_str(),
                    "final_expiry_delta": hex_u64(final_expiry_delta_ms),
                    "expiry": hex_u64(expiry_secs),
                    "description": description,
                })),
            )
            .await?;
        let invoice = &result["invoice"];
        let got_hash = super::payment_hash_from_invoice(invoice)?;
        if !super::same_hash(&got_hash, payment_hash) {
            bail!("hold invoice hash {got_hash} does not match {payment_hash}");
        }
        let got_algorithm = super::hash_algorithm_from_invoice(invoice)?;
        if got_algorithm != hash_algorithm {
            bail!(
                "hold invoice algorithm {} does not match {}",
                got_algorithm.as_str(),
                hash_algorithm.as_str()
            );
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

    async fn new_payout_invoice(
        &self,
        amount: u128,
        description: &str,
        final_expiry_delta_ms: u64,
    ) -> Result<InvoiceCreated> {
        let result = self
            .call(
                "new_invoice",
                Some(json!({
                    "amount": hex_u128(amount),
                    "currency": self.currency,
                    "final_expiry_delta": hex_u64(final_expiry_delta_ms),
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
        let result = match self
            .call("parse_invoice", Some(json!({ "invoice": invoice })))
            .await
        {
            Ok(result) => result,
            // Fiber v0.9.1 cannot read a trampoline route hint; read it here instead.
            Err(error) => match super::hint::decode_hinted_invoice(invoice) {
                Ok(hinted) => return parsed_from_hinted(&hinted),
                Err(_) => return Err(error),
            },
        };
        let invoice = &result["invoice"];
        Ok(ParsedInvoice {
            payment_hash: super::payment_hash_from_invoice(invoice)?,
            currency: invoice
                .get("currency")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            amount: super::hex_amount(invoice.get("amount"))?,
            hash_algorithm: super::hash_algorithm_from_invoice(invoice)?,
            final_tlc_expiry_delta_ms: super::final_tlc_expiry_delta_ms(invoice)?,
            expires_at_ms: super::invoice_expires_at_ms(invoice)?,
        })
    }
}

fn parsed_from_hinted(hinted: &super::HintedInvoice) -> Result<ParsedInvoice> {
    let hash_algorithm = match hinted.hash_algorithm {
        0 => HashAlgorithm::CkbHash,
        1 => HashAlgorithm::Sha256,
        other => bail!("unsupported hash algorithm {other}"),
    };
    let expires_at_ms = match hinted.expiry_secs {
        None => None,
        Some(secs) => {
            let at = hinted
                .timestamp_ms
                .checked_add(u128::from(secs) * 1_000)
                .ok_or_else(|| anyhow!("invoice expiry is too large"))?;
            Some(i64::try_from(at).map_err(|_| anyhow!("invoice expiry is too large"))?)
        }
    };
    Ok(ParsedInvoice {
        payment_hash: hinted.payment_hash.clone(),
        currency: hinted.currency.clone(),
        amount: hinted.amount,
        hash_algorithm,
        // Paid through a trampoline, so the payout needs more lock than the payee asks.
        final_tlc_expiry_delta_ms: hinted
            .final_tlc_expiry_delta_ms
            .unwrap_or(crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS)
            .saturating_add(crate::TRAMPOLINE_SLACK_MS),
        expires_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hinted_invoice_counts_the_trampoline_lock() {
        let hinted = super::super::HintedInvoice {
            currency: "Fibt".into(),
            amount: Some(10_000_000_000),
            timestamp_ms: 1_000,
            payment_hash: format!("0x{}", "11".repeat(32)),
            expiry_secs: Some(172_800),
            final_tlc_expiry_delta_ms: Some(86_400_000),
            payee: format!("02{}", "22".repeat(32)),
            hash_algorithm: 0,
            trampoline_hint: format!("03{}", "33".repeat(32)),
        };
        let parsed = parsed_from_hinted(&hinted).unwrap();
        // 24h asked by the wallet, about 43h needed through the trampoline (measured).
        assert_eq!(
            parsed.final_tlc_expiry_delta_ms,
            86_400_000 + crate::TRAMPOLINE_SLACK_MS
        );
        assert!(parsed.final_tlc_expiry_delta_ms >= 42 * 3_600_000 + 48 * 60_000);
        assert_eq!(parsed.expires_at_ms, Some(1_000 + 172_800_000));
    }
}
