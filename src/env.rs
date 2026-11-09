use crate::constant::{
    DEFAULT_DB_PATH, DEV_INVOICE_CURRENCY, MAINNET_INVOICE_CURRENCY, TESTNET_INVOICE_CURRENCY,
};
use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::PublicKey;
use std::path::PathBuf;

fn canonical_solver(value: &str) -> Result<Option<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let key = PublicKey::parse(trimmed).context("TWINE_SOLVER must be a hex pubkey or npub")?;
    Ok(Some(key.to_hex()))
}

#[derive(Debug, Clone)]
pub struct Config {
    pub network: String,
    pub invoice_currency: String,
    pub nostr_secret: String,
    pub relays: Vec<String>,
    pub rpc_url: String,
    pub rpc_token: String,
    pub db_path: PathBuf,
    pub solver: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let nostr_secret =
            std::env::var("TWINE_NOSTR_SECRET").context("TWINE_NOSTR_SECRET is required")?;
        let relays = std::env::var("TWINE_RELAYS")
            .context("TWINE_RELAYS is required")?
            .split(',')
            .map(|relay| relay.trim().to_string())
            .filter(|relay| !relay.is_empty())
            .collect::<Vec<_>>();
        if relays.is_empty() {
            bail!("TWINE_RELAYS must list at least one relay");
        }
        let rpc_url = std::env::var("TWINE_RPC").context("TWINE_RPC is required")?;
        let rpc_token = std::env::var("TWINE_RPC_TOKEN")
            .context("TWINE_RPC_TOKEN is required")?
            .trim()
            .to_string();
        if rpc_token.is_empty() {
            bail!("TWINE_RPC_TOKEN is required");
        }
        let db_path = std::env::var("TWINE_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB_PATH));
        let network = std::env::var("FIBER_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let currency = invoice_currency(network.trim())?.to_string();
        let network = network.trim().to_string();
        let solver = match std::env::var("TWINE_SOLVER") {
            Ok(value) => canonical_solver(&value)?,
            Err(_) => None,
        };
        Ok(Self {
            network,
            invoice_currency: currency,
            nostr_secret,
            relays,
            rpc_url,
            rpc_token,
            db_path,
            solver,
        })
    }
}

pub fn invoice_currency(network: &str) -> Result<&'static str> {
    match network {
        "testnet" => Ok(TESTNET_INVOICE_CURRENCY),
        "mainnet" => Ok(MAINNET_INVOICE_CURRENCY),
        "dev" => Ok(DEV_INVOICE_CURRENCY),
        other => bail!("FIBER_NETWORK must be testnet, mainnet, or dev, got {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_solver, invoice_currency};
    use nostr_sdk::prelude::{Keys, ToBech32};

    #[test]
    fn invoice_currency_follows_fiber_network() {
        assert_eq!(invoice_currency("testnet").unwrap(), "Fibt");
        assert_eq!(invoice_currency("mainnet").unwrap(), "Fibb");
        assert_eq!(invoice_currency("dev").unwrap(), "Fibd");
        assert!(invoice_currency("devnet").is_err());
    }

    #[test]
    fn solver_pubkey_is_canonical_hex() {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex();
        let npub = keys.public_key().to_bech32().unwrap();
        assert_eq!(
            canonical_solver(&hex.to_uppercase()).unwrap().as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            canonical_solver(&npub).unwrap().as_deref(),
            Some(hex.as_str())
        );
        assert!(canonical_solver("  ").unwrap().is_none());
        assert!(canonical_solver("not-a-key").is_err());
    }
}
