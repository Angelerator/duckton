//! `p2p-transport` — QUIC (Quinn + rustls) transport with mutual TLS pinned to
//! Ed25519 node identities (architecture §5/§6).
//!
//! Highlights:
//!  * Self-signed certs bound to the node's Ed25519 key; peers derive `node_id`
//!    from the presented cert (`b3:` BLAKE3 of the public key).
//!  * Pinning policy is pluggable (TOFU / allowlist) and configured, not hard-coded.
//!  * Everything async on tokio; multiplexed bi-streams + datagrams.
//!  * The [`Transport`] trait abstracts the concrete [`QuicTransport`] so node
//!    logic can be tested against fakes and alternative transports swapped in.

pub mod endpoint;
pub mod error;
pub mod identity;
pub mod verifier;
pub mod version;

pub use endpoint::{read_msg, request_response, write_msg, Conn, QuicTransport, Transport};
// Re-export the QUIC stream types so dependents don't need a direct `quinn` dep.
pub use quinn::{RecvStream, SendStream};
pub use error::{Result, TransportError};
pub use identity::NodeIdentity;
pub use verifier::{node_id_from_cert, PinPolicy};
pub use version::{Negotiated, VersionInfo};
