//! Local deny/allow lists keyed by `node_id` / `wallet` (ARCHITECTURE "Abuse
//! resistance"). Managed via the SQL admin surface (`p2p_block`, `p2p_unblock`,
//! `p2p_blocklist`) and persisted to a dedicated `blocklist.toml` alongside the
//! runtime config.
//!
//! Each node maintains its **own** blocklist and decides independently whom to
//! refuse — there is no central authority. Entries may be added manually, by an
//! auto-block trigger (trust floor / slashing record), or by honoring a signed,
//! gossiped abuse signal. An optional governance blocklist in the on-chain
//! `GlobalParams` contract covers egregious provable cases.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::value::Table;
use toml::Value;

use crate::store::StoreError;

/// What an identifier refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// A `b3:` node identity.
    NodeId,
    /// A wallet address.
    Wallet,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::NodeId => "node_id",
            BlockKind::Wallet => "wallet",
        }
    }

    /// Parse a kind string (lenient). Anything starting `b3:` defaults to
    /// `node_id` at the call site; this only parses an explicit kind.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "node_id" | "node" | "nodeid" | "peer" => Some(BlockKind::NodeId),
            "wallet" | "address" | "addr" => Some(BlockKind::Wallet),
            _ => None,
        }
    }
}

/// One persisted deny-list entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEntry {
    pub id: String,
    pub kind: BlockKind,
    pub reason: String,
    pub source: String,
    pub ts: u64,
}

/// Persisted deny-list store. Mirrors [`crate::ConfigStore`] persistence
/// conventions (own file, friendly errors, never panics on bad input).
pub struct BlocklistStore {
    path: PathBuf,
}

impl BlocklistStore {
    /// Open the store at the default location (`<config-dir>/blocklist.toml`, or
    /// `$P2P_BLOCKLIST` if set).
    pub fn open() -> Self {
        let path = std::env::var("P2P_BLOCKLIST")
            .map(PathBuf::from)
            .unwrap_or_else(|_| crate::store::default_config_dir().join("blocklist.toml"));
        Self { path }
    }

    /// Construct with an explicit path (tests).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<Vec<BlockEntry>, StoreError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };
        let table: Table = toml::from_str(&text)
            .map_err(|e| StoreError::BadParam(format!("blocklist file is corrupt: {e}")))?;
        let mut out = Vec::new();
        if let Some(Value::Array(entries)) = table.get("entry") {
            for v in entries {
                if let Value::Table(t) = v {
                    let id = t
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let kind = t
                        .get("kind")
                        .and_then(Value::as_str)
                        .and_then(BlockKind::parse)
                        .unwrap_or(BlockKind::NodeId);
                    let reason = t
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let source = t
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("manual")
                        .to_string();
                    let ts = t.get("ts").and_then(Value::as_integer).unwrap_or(0) as u64;
                    out.push(BlockEntry {
                        id,
                        kind,
                        reason,
                        source,
                        ts,
                    });
                }
            }
        }
        Ok(out)
    }

    fn write(&self, entries: &[BlockEntry]) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StoreError::Io(parent.display().to_string(), e.to_string()))?;
        }
        let mut root = Table::new();
        let arr: Vec<Value> = entries
            .iter()
            .map(|e| {
                let mut t = BTreeMap::new();
                t.insert("id".to_string(), Value::String(e.id.clone()));
                t.insert(
                    "kind".to_string(),
                    Value::String(e.kind.as_str().to_string()),
                );
                t.insert("reason".to_string(), Value::String(e.reason.clone()));
                t.insert("source".to_string(), Value::String(e.source.clone()));
                t.insert("ts".to_string(), Value::Integer(e.ts as i64));
                Value::Table(t.into_iter().collect())
            })
            .collect();
        root.insert("entry".to_string(), Value::Array(arr));
        let text = toml::to_string_pretty(&Value::Table(root))
            .map_err(|e| StoreError::BadParam(format!("serialize blocklist: {e}")))?;
        std::fs::write(&self.path, text)
            .map_err(|e| StoreError::Io(self.path.display().to_string(), e.to_string()))?;
        Ok(())
    }

    /// All current entries.
    pub fn list(&self) -> Result<Vec<BlockEntry>, StoreError> {
        self.read()
    }

    /// Whether an identifier is currently blocked.
    pub fn is_blocked(&self, id: &str) -> Result<bool, StoreError> {
        Ok(self.read()?.iter().any(|e| e.id == id))
    }

    /// Add (or refresh) a deny-list entry. Idempotent on `id`.
    pub fn block(
        &self,
        id: &str,
        kind: BlockKind,
        reason: &str,
        source: &str,
        ts: u64,
    ) -> Result<(), StoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(StoreError::BadParam("block: id must be non-empty".into()));
        }
        let mut entries = self.read()?;
        entries.retain(|e| e.id != id);
        entries.push(BlockEntry {
            id: id.to_string(),
            kind,
            reason: reason.to_string(),
            source: source.to_string(),
            ts,
        });
        self.write(&entries)
    }

    /// Remove a deny-list entry. Returns whether something was removed.
    pub fn unblock(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.trim();
        let mut entries = self.read()?;
        let before = entries.len();
        entries.retain(|e| e.id != id);
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

    fn store() -> (BlocklistStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            BlocklistStore::with_path(dir.path().join("blocklist.toml")),
            dir,
        )
    }

    #[test]
    fn block_unblock_roundtrip_persists() {
        let (s, dir) = store();
        assert!(s.list().unwrap().is_empty());
        s.block("b3:bad", BlockKind::NodeId, "cheating", "manual", 10)
            .unwrap();
        assert!(s.is_blocked("b3:bad").unwrap());
        // survives reopen
        let reopened = BlocklistStore::with_path(dir.path().join("blocklist.toml"));
        assert!(reopened.is_blocked("b3:bad").unwrap());
        assert_eq!(reopened.list().unwrap().len(), 1);
        assert!(reopened.unblock("b3:bad").unwrap());
        assert!(!reopened.is_blocked("b3:bad").unwrap());
    }

    #[test]
    fn block_is_idempotent_on_id() {
        let (s, _d) = store();
        s.block("kQwallet", BlockKind::Wallet, "r1", "manual", 1)
            .unwrap();
        s.block("kQwallet", BlockKind::Wallet, "r2", "auto", 2)
            .unwrap();
        let entries = s.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].reason, "r2");
        assert_eq!(entries[0].source, "auto");
    }

    #[test]
    fn unblock_missing_is_noop() {
        let (s, _d) = store();
        assert!(!s.unblock("nope").unwrap());
    }
}
