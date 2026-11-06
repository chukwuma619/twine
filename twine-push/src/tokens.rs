use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Default)]
pub struct TokenStore {
    path: Option<PathBuf>,
    tokens: HashMap<String, HashSet<String>>,
}

impl TokenStore {
    pub fn memory() -> Self {
        Self::default()
    }

    pub fn load(path: PathBuf) -> Self {
        let tokens = match std::fs::read_to_string(&path) {
            Ok(raw) => parse_tokens(&raw).unwrap_or_else(|error| {
                warn!(%error, path = %path.display(), "token file unreadable; starting empty");
                HashMap::new()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                warn!(%error, path = %path.display(), "token file missing; starting empty");
                HashMap::new()
            }
        };
        Self {
            path: Some(path),
            tokens,
        }
    }

    pub fn add(&mut self, pubkey: String, token: String) {
        self.tokens.entry(pubkey).or_default().insert(token);
        self.try_persist();
    }

    pub fn remove(&mut self, pubkey: &str, token: &str) {
        if let Some(set) = self.tokens.get_mut(pubkey) {
            set.remove(token);
            if set.is_empty() {
                self.tokens.remove(pubkey);
            }
        }
        self.try_persist();
    }

    pub fn drop_token(&mut self, token: &str) {
        self.tokens.retain(|_, set| {
            set.remove(token);
            !set.is_empty()
        });
        self.try_persist();
    }

    pub fn tokens_for<'a>(&'a self, pubkey: &str) -> impl Iterator<Item = &'a String> {
        self.tokens
            .get(pubkey)
            .into_iter()
            .flat_map(|set| set.iter())
    }

    fn try_persist(&self) {
        if let Err(error) = self.persist() {
            warn!(%error, "token persist failed");
        }
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.tokens)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

fn parse_tokens(raw: &str) -> Result<HashMap<String, HashSet<String>>> {
    let value: Value = serde_json::from_str(raw).context("invalid token JSON")?;
    let object = value.as_object().context("token JSON must be an object")?;
    let mut tokens = HashMap::new();
    for (pubkey, entry) in object {
        let Some(list) = entry.as_array() else {
            continue;
        };
        let set = list
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .filter(|token| !token.is_empty())
            .collect::<HashSet<_>>();
        if !set.is_empty() {
            tokens.insert(pubkey.clone(), set);
        }
    }
    Ok(tokens)
}

pub fn tokens_path_from_env() -> PathBuf {
    std::env::var("TWINE_PUSH_TOKENS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("twine-push-tokens.json"))
}

#[cfg(test)]
fn roundtrip_file(path: PathBuf, pubkey: &str, token: &str) -> Result<TokenStore> {
    let mut store = TokenStore::load(path.clone());
    store.add(pubkey.to_string(), token.to_string());
    Ok(TokenStore::load(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!("twine-push-tokens-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tokens.json");
        let _ = std::fs::remove_file(&path);
        let reloaded = roundtrip_file(path.clone(), "abc", "device-token").unwrap();
        assert!(reloaded.tokens_for("abc").any(|token| token == "device-token"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn drop_token_removes_it_from_every_pubkey() {
        let mut store = TokenStore::memory();
        store.add("a".into(), "shared".into());
        store.add("b".into(), "shared".into());
        store.add("b".into(), "other".into());
        store.drop_token("shared");
        assert_eq!(store.tokens_for("a").count(), 0);
        assert_eq!(store.tokens_for("b").count(), 1);
        assert!(store.tokens_for("b").any(|token| token == "other"));
    }
}
