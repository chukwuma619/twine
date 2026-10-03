use crate::constant::{DEFAULT_DB_PATH, MAINNET_INVOICE_CURRENCY, TESTNET_INVOICE_CURRENCY};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub network: String,
    pub invoice_currency: String,
    pub nostr_secret: String,
    pub relays: Vec<String>,
    pub rpc_url: String,
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
        let db_path = std::env::var("TWINE_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DB_PATH));
        let network = std::env::var("FIBER_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let currency = invoice_currency(network.trim())?.to_string();
        let network = network.trim().to_string();
        let solver = std::env::var("TWINE_SOLVER")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Ok(Self {
            network,
            invoice_currency: currency,
            nostr_secret,
            relays,
            rpc_url,
            db_path,
            solver,
        })
    }
}

pub fn invoice_currency(network: &str) -> Result<&'static str> {
    match network {
        "testnet" => Ok(TESTNET_INVOICE_CURRENCY),
        "mainnet" => Ok(MAINNET_INVOICE_CURRENCY),
        other => bail!("FIBER_NETWORK must be testnet or mainnet, got {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::invoice_currency;

    #[test]
    fn invoice_currency_follows_fiber_network() {
        assert_eq!(invoice_currency("testnet").unwrap(), "Fibt");
        assert_eq!(invoice_currency("mainnet").unwrap(), "Fibb");
        assert!(invoice_currency("devnet").is_err());
    }
}
