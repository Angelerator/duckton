//! Signed capability advertisement (architecture §8).
//!
//! Workers periodically publish a signed capability record to the gossip/DHT
//! layer:  `{node_id, free_mem, free_cores, max_jobs, attestation_level, price,
//! recent_receipts_root}`. Requesters filter these locally by capacity + trust +
//! attestation and pick candidates — never contacting the whole swarm.
//!
//! The proto layer only carries the data + the PoW fields + the signature; the
//! `p2p-trust` layer signs and verifies it (it owns the crypto).

use serde::{Deserialize, Serialize};

use crate::attestation::AttestationLevel;
use crate::ids::NodeId;
use crate::version::Version;

/// A signed advertisement of a worker's current capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAd {
    /// Wire schema tag of this record (versioned gossip record).
    pub schema_version: u16,
    /// Protocol version the advertising node speaks.
    pub protocol_version: Version,
    pub node_id: NodeId,
    /// Hex Ed25519 public key (so verifiers can derive node_id and check `sig`).
    pub pubkey: String,
    /// Reachable QUIC address (host:port).
    pub addr: String,
    pub free_mem_bytes: u64,
    pub free_threads: u32,
    pub max_jobs: u32,
    pub attestation_level: AttestationLevel,
    pub price: u64,
    /// Root hash (hex) of the worker's recent receipts bundle (optional).
    pub recent_receipts_root: Option<String>,
    /// Proof-of-work nonce binding this identity (Sybil resistance).
    pub pow_nonce: u64,
    /// Claimed PoW difficulty (leading zero bits).
    pub pow_bits: u32,
    /// Unix-seconds timestamp (freshness; stale ads are dropped).
    pub ts: u64,
    /// Hex Ed25519 signature over the canonical signing bytes.
    pub sig: String,
}

/// A node's **durable, self-measured** capability profile (the empirical
/// "what this node has really pulled off" record, architecture §3 of the
/// routing design). Distinct from the ephemeral [`CapabilityAd`] ("free
/// resources now"): this is a compact, all-time, monotonic aggregate persisted
/// across restarts. It is Ed25519-signed + node-id-bound (provenance/integrity)
/// and carries a strictly-increasing `seq` so a rollback to an older snapshot is
/// detectable. Signing/verification live in `p2p-trust::capability`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityProfile {
    /// Wire schema tag of this record.
    pub schema_version: u16,
    pub node_id: NodeId,
    /// Hex Ed25519 public key (so verifiers can derive node_id and check `sig`).
    pub pubkey: String,
    /// All-time maxima from REAL successful local executions (ratchet up only).
    pub max_input_bytes: u64,
    pub max_result_rows: u64,
    pub max_result_bytes: u64,
    /// Largest peak buffer memory / temp-dir (spill) that still succeeded.
    pub max_peak_memory_bytes: u64,
    pub max_temp_dir_bytes: u64,
    /// Count of successful local executions backing the maxima.
    pub successes: u64,
    /// Strictly-increasing update counter (rollback/monotonicity guard).
    pub seq: u64,
    /// Unix-seconds timestamp of the last update.
    pub ts: u64,
    /// Hex Ed25519 signature over the canonical signing bytes.
    pub sig: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_ad_roundtrips() {
        let ad = CapabilityAd {
            schema_version: crate::SCHEMA_VERSION,
            protocol_version: crate::PROTOCOL_VERSION,
            node_id: NodeId("b3:w".into()),
            pubkey: "00".repeat(32),
            addr: "127.0.0.1:9494".into(),
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            max_jobs: 3,
            attestation_level: AttestationLevel::L0,
            price: 0,
            recent_receipts_root: None,
            pow_nonce: 7,
            pow_bits: 16,
            ts: 100,
            sig: "ab".repeat(32),
        };
        let bytes = crate::to_bytes(&ad).unwrap();
        let back: CapabilityAd = crate::from_bytes(&bytes).unwrap();
        assert_eq!(ad, back);
    }
}
