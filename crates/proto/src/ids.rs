//! Identity and hash-derived identifier types.

use serde::{Deserialize, Serialize};

/// A node identity: `BLAKE3(ed25519_public_key)` rendered as lowercase hex.
///
/// In the architecture doc this is `multibase(BLAKE3(public_key))`; we use a
/// plain hex encoding here (a `b3:` style prefix is added by [`NodeId::from_pubkey`])
/// which is unambiguous and trivially comparable. The transport layer pins
/// connections to a `NodeId` derived from the peer's presented certificate key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    /// Derive a `NodeId` from raw Ed25519 public key bytes (32 bytes).
    pub fn from_pubkey(pubkey: &[u8]) -> Self {
        let hash = blake3::hash(pubkey);
        NodeId(format!("b3:{}", hex::encode(hash.as_bytes())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A job identifier (random, unique per dispatch attempt set).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    /// Create a fresh random job id.
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        rng_fill(&mut bytes);
        JobId(hex::encode(bytes))
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A query hash binding the SQL text + the DuckDB version it is intended for.
///
/// Per §15 of the architecture doc, canonical result determinism depends on the
/// engine version, so the version is folded into the query hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryHash(pub String);

impl QueryHash {
    pub fn compute(sql: &str, engine_version: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"duckdb-p2p-query-v1");
        hasher.update(engine_version.as_bytes());
        hasher.update(&[0]);
        hasher.update(sql.as_bytes());
        QueryHash(hex::encode(hasher.finalize().as_bytes()))
    }
}

/// Fill a buffer with cryptographically-random bytes via `getrandom`.
fn rng_fill(buf: &mut [u8]) {
    use rand::RngCore;
    rand::rngs::OsRng.fill_bytes(buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_is_stable_for_same_pubkey() {
        let pk = [7u8; 32];
        assert_eq!(NodeId::from_pubkey(&pk), NodeId::from_pubkey(&pk));
        assert_ne!(NodeId::from_pubkey(&pk), NodeId::from_pubkey(&[8u8; 32]));
        assert!(NodeId::from_pubkey(&pk).as_str().starts_with("b3:"));
    }

    #[test]
    fn query_hash_folds_in_version() {
        let a = QueryHash::compute("SELECT 1", "1.0");
        let b = QueryHash::compute("SELECT 1", "2.0");
        assert_ne!(a, b, "different engine versions must hash differently");
        assert_eq!(a, QueryHash::compute("SELECT 1", "1.0"));
    }

    #[test]
    fn job_ids_are_unique() {
        assert_ne!(JobId::new(), JobId::new());
    }
}
