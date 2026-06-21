//! Persistent **node↔wallet binding source** (BLOCKCHAIN_ECONOMICS §3, §8).
//!
//! Maps a `b3:` node identity to its operator wallet (and, when known, the
//! wallet↔node binding hash used at `StakeVault` deploy). This is the local
//! "directory" a requester consults to resolve a peer's payout wallet and its
//! per-node vault address, so:
//!   * the coordinator can direct real settlement payouts to the bound wallet
//!     (instead of the deterministic `blake3(node_id)` placeholder), and
//!   * a live `TonStakeRegistry` can derive each peer's per-node `StakeVault`
//!     address (it needs the owner wallet + binding hash) and read its on-chain
//!     stake — so `require_staked_hosts` / `sybil.min_stake` / the paid
//!     `stake_factor` reflect REAL stake.
//!
//! Mirrors [`crate::BlocklistStore`] conventions (own TOML file, friendly errors,
//! never panics on bad input). Empty by default ⇒ inert (the placeholder/no-stake
//! behavior is unchanged). Entries are added by an operator/admin surface or by a
//! verified, gossiped `NodeWalletBinding` (the wire side is settlement-layer work).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::value::Table;
use toml::Value;

use crate::store::StoreError;

/// One persisted node↔wallet binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingEntry {
    /// `b3:` node identity.
    pub node_id: String,
    /// Operator wallet address (raw `wc:hex` or user-friendly base64 form).
    pub wallet: String,
    /// Hex-encoded wallet↔node binding hash stored at vault deploy (§3.2). Empty
    /// when only the wallet is known (sufficient for payout routing, but the
    /// per-node vault address derivation also needs this hash).
    pub binding_hash: String,
    pub ts: u64,
}

/// Persisted node↔wallet binding store.
pub struct BindingStore {
    path: PathBuf,
}

impl BindingStore {
    /// Open at the default location (`<config-dir>/bindings.toml`, or
    /// `$P2P_BINDINGS` if set).
    pub fn open() -> Self {
        let path = std::env::var("P2P_BINDINGS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::store::default_config_dir().join("bindings.toml"));
        Self { path }
    }

    /// Construct with an explicit path (tests).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<Vec<BindingEntry>, StoreError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };
        let table: Table = toml::from_str(&text)
            .map_err(|e| StoreError::BadParam(format!("bindings file is corrupt: {e}")))?;
        let mut out = Vec::new();
        if let Some(Value::Array(entries)) = table.get("entry") {
            for v in entries {
                if let Value::Table(t) = v {
                    let node_id = t
                        .get("node_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let wallet = t
                        .get("wallet")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if node_id.is_empty() || wallet.is_empty() {
                        continue;
                    }
                    let binding_hash = t
                        .get("binding_hash")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let ts = t.get("ts").and_then(Value::as_integer).unwrap_or(0) as u64;
                    out.push(BindingEntry {
                        node_id,
                        wallet,
                        binding_hash,
                        ts,
                    });
                }
            }
        }
        Ok(out)
    }

    fn write(&self, entries: &[BindingEntry]) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Io(parent.display().to_string(), e.to_string()))?;
        }
        let mut root = Table::new();
        let arr: Vec<Value> = entries
            .iter()
            .map(|e| {
                let mut t = BTreeMap::new();
                t.insert("node_id".to_string(), Value::String(e.node_id.clone()));
                t.insert("wallet".to_string(), Value::String(e.wallet.clone()));
                t.insert(
                    "binding_hash".to_string(),
                    Value::String(e.binding_hash.clone()),
                );
                t.insert("ts".to_string(), Value::Integer(e.ts as i64));
                Value::Table(t.into_iter().collect())
            })
            .collect();
        root.insert("entry".to_string(), Value::Array(arr));
        let text = toml::to_string_pretty(&Value::Table(root))
            .map_err(|e| StoreError::BadParam(format!("serialize bindings: {e}")))?;
        std::fs::write(&self.path, text)
            .map_err(|e| StoreError::Io(self.path.display().to_string(), e.to_string()))?;
        Ok(())
    }

    /// All current bindings.
    pub fn list(&self) -> Result<Vec<BindingEntry>, StoreError> {
        self.read()
    }

    /// The binding for `node_id`, if any.
    pub fn get(&self, node_id: &str) -> Result<Option<BindingEntry>, StoreError> {
        Ok(self.read()?.into_iter().find(|e| e.node_id == node_id))
    }

    /// Add (or refresh) a binding. Idempotent on `node_id`.
    pub fn bind(
        &self,
        node_id: &str,
        wallet: &str,
        binding_hash: &str,
        ts: u64,
    ) -> Result<(), StoreError> {
        let node_id = node_id.trim();
        let wallet = wallet.trim();
        if node_id.is_empty() || wallet.is_empty() {
            return Err(StoreError::BadParam(
                "bind: node_id and wallet must be non-empty".into(),
            ));
        }
        let mut entries = self.read()?;
        entries.retain(|e| e.node_id != node_id);
        entries.push(BindingEntry {
            node_id: node_id.to_string(),
            wallet: wallet.to_string(),
            binding_hash: binding_hash.trim().to_string(),
            ts,
        });
        self.write(&entries)
    }

    /// Remove a binding. Returns whether something was removed.
    pub fn unbind(&self, node_id: &str) -> Result<bool, StoreError> {
        let node_id = node_id.trim();
        let mut entries = self.read()?;
        let before = entries.len();
        entries.retain(|e| e.node_id != node_id);
        let removed = entries.len() != before;
        if removed {
            self.write(&entries)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (BindingStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            BindingStore::with_path(dir.path().join("bindings.toml")),
            dir,
        )
    }

    #[test]
    fn bind_get_unbind_roundtrip_persists() {
        let (s, dir) = store();
        assert!(s.list().unwrap().is_empty());
        s.bind("b3:w", "kQwallet", "abcd", 10).unwrap();
        let got = s.get("b3:w").unwrap().expect("bound");
        assert_eq!(got.wallet, "kQwallet");
        assert_eq!(got.binding_hash, "abcd");
        // Survives reopen.
        let reopened = BindingStore::with_path(dir.path().join("bindings.toml"));
        assert_eq!(reopened.list().unwrap().len(), 1);
        assert!(reopened.unbind("b3:w").unwrap());
        assert!(reopened.get("b3:w").unwrap().is_none());
    }

    #[test]
    fn bind_is_idempotent_on_node_id() {
        let (s, _d) = store();
        s.bind("b3:w", "kQ1", "", 1).unwrap();
        s.bind("b3:w", "kQ2", "ff", 2).unwrap();
        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].wallet, "kQ2");
        assert_eq!(entries[0].binding_hash, "ff");
    }

    #[test]
    fn rejects_empty_and_unbind_missing_is_noop() {
        let (s, _d) = store();
        assert!(s.bind("", "kQ", "", 1).is_err());
        assert!(s.bind("b3:w", "", "", 1).is_err());
        assert!(!s.unbind("nope").unwrap());
    }
}
