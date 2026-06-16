//! `p2p-proto` — shared protocol types for the distributed P2P DuckDB grid.
//!
//! This crate defines the on-the-wire message types (Offer / Bid / Dispatch /
//! result framing / receipts), the node identity type, the attestation model,
//! and a portable SQL value/result model used for canonical result hashing and
//! by the mock query engine.
//!
//! Serialization uses `serde` with a JSON wire form by default. JSON keeps the
//! control plane debuggable; bulk result chunks carry opaque byte payloads so
//! the heavy path is not JSON-encoded field-by-field.

pub mod attestation;
pub mod capability;
pub mod ids;
pub mod messages;
pub mod value;
pub mod version;

pub use attestation::{Attestation, AttestationLevel};
pub use capability::{CapabilityAd, CapabilityProfile};
pub use ids::{JobId, NodeId, QueryHash};
pub use messages::*;
pub use value::{ResultSet, Value};
pub use version::{
    alpn_for_major, current_alpn, Version, MIN_SUPPORTED_VERSION, PROTOCOL_NAME, PROTOCOL_VERSION,
    SCHEMA_VERSION,
};

/// Errors produced when (de)serializing protocol messages.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("frame too large: {0} bytes (max {1})")]
    FrameTooLarge(usize, usize),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

/// Maximum size of a single control-plane frame (4 MiB). Bulk results use
/// dedicated chunk messages, so control frames should never approach this.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Absolute upper bound on a single transferred result payload (8 GiB).
///
/// This is a hard, defense-in-depth ceiling on the **attacker-controlled** size
/// fields carried in a [`messages::ResultManifest`] (`total_len` /
/// `uncompressed_len`). The bulk result path deliberately transfers bytes
/// *outside* the [`MAX_FRAME_BYTES`] control-frame cap, so without this ceiling a
/// malicious/compromised winning worker could declare a huge size and drive the
/// receiver to pre-allocate unbounded memory (OOM). Receivers SHOULD additionally
/// impose a tighter, configurable per-job cap; see
/// [`messages::ResultManifest::validate`].
pub const MAX_RESULT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Absolute upper bound on the number of parallel result streams a manifest may
/// declare — defense-in-depth against an unbounded `accept_uni` accept loop on
/// the receiver driven by an attacker-supplied `parts` count.
pub const MAX_RESULT_PARTS: u32 = 4096;

/// Encode a serializable value to its canonical JSON wire bytes.
pub fn to_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    Ok(serde_json::to_vec(value)?)
}

/// Decode a value from JSON wire bytes.
///
/// SAFETY/DoS NOTE: this assumes `bytes` is already length-capped by the caller
/// (the framed transport path enforces [`MAX_FRAME_BYTES`] before calling this).
/// `serde_json` recurses on nested structures, so feeding it an arbitrarily large
/// or deeply-nested *un-capped* buffer can exhaust memory / the stack. Do not call
/// this on un-framed, attacker-controlled input without a prior size bound.
pub fn from_bytes<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    Ok(serde_json::from_slice(bytes)?)
}
