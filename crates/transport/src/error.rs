//! Transport error type.

pub type Result<T> = std::result::Result<T, TransportError>;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("identity error: {0}")]
    Identity(String),
    #[error("tls/crypto error: {0}")]
    Tls(String),
    #[error("endpoint error: {0}")]
    Endpoint(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("peer pinning failure: expected {expected}, got {actual}")]
    Pinning { expected: String, actual: String },
    #[error("incompatible protocol version: {0}")]
    IncompatibleVersion(String),
    #[error("wire schema mismatch: got {got}, expected {expected}")]
    SchemaMismatch { got: u16, expected: u16 },
    #[error("stream i/o error: {0}")]
    Stream(String),
    #[error("protocol error: {0}")]
    Proto(#[from] p2p_proto::ProtoError),
    #[error("frame too large: {0} bytes (max {1})")]
    FrameTooLarge(usize, usize),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
