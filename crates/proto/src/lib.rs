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
pub use capability::CapabilityAd;
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

/// Encode a serializable value to its canonical JSON wire bytes.
pub fn to_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    Ok(serde_json::to_vec(value)?)
}

/// Decode a value from JSON wire bytes.
pub fn from_bytes<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    Ok(serde_json::from_slice(bytes)?)
}
