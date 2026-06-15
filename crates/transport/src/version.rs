//! Protocol version negotiation policy (architecture §5.1).
//!
//! Holds the local node's advertised version + minimum supported version +
//! engine/extension build versions, and implements the compatibility check
//! applied during the connection handshake. Everything is configurable via
//! [`p2p_config::ProtocolConfig`] — no magic constants.

use p2p_config::ProtocolConfig;
use p2p_proto::version::Version;
use p2p_proto::{Hello, NodeId, MIN_SUPPORTED_VERSION, PROTOCOL_VERSION, SCHEMA_VERSION};

use crate::error::TransportError;

/// Local versioning info advertised in the handshake and used to evaluate peers.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub version: Version,
    pub min_supported: Version,
    pub engine_version: String,
    pub extension_version: String,
    /// If true, peers must report the same `engine_version` to be compatible
    /// (result-determinism for quorum). Empty local engine version disables it.
    pub require_matching_engine: bool,
}

impl Default for VersionInfo {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            min_supported: MIN_SUPPORTED_VERSION,
            engine_version: String::new(),
            extension_version: env!("CARGO_PKG_VERSION").to_string(),
            require_matching_engine: false,
        }
    }
}

impl VersionInfo {
    /// Build from configuration + the node's engine version string.
    pub fn from_config(
        cfg: &ProtocolConfig,
        engine_version: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let version: Version = cfg
            .version
            .parse()
            .map_err(|e| TransportError::IncompatibleVersion(format!("bad protocol version: {e}")))?;
        let min_supported: Version = cfg.min_supported_version.parse().map_err(|e| {
            TransportError::IncompatibleVersion(format!("bad min_supported_version: {e}"))
        })?;
        Ok(Self {
            version,
            min_supported,
            engine_version: engine_version.into(),
            extension_version: env!("CARGO_PKG_VERSION").to_string(),
            require_matching_engine: cfg.require_matching_engine_version,
        })
    }

    /// The `Hello` message advertising this node.
    pub fn hello(&self, node_id: NodeId) -> Hello {
        Hello {
            schema_version: SCHEMA_VERSION,
            protocol_version: self.version,
            min_supported: self.min_supported,
            node_id,
            engine_version: self.engine_version.clone(),
            extension_version: self.extension_version.clone(),
        }
    }

    /// Evaluate a peer's `Hello` against the compatibility policy.
    ///
    /// On success returns the negotiated common version (the lower of the two,
    /// since the newer side downgrades). On failure returns a typed error with a
    /// human-readable reason.
    pub fn negotiate(&self, peer: &Hello) -> Result<Negotiated, TransportError> {
        // Wire schema tag must match (defense-in-depth on top of the per-frame
        // schema check). The field carried in `Hello` is otherwise ignored, so a
        // peer could advertise any schema with no effect; validate it here.
        if peer.schema_version != SCHEMA_VERSION {
            return Err(TransportError::IncompatibleVersion(format!(
                "schema version mismatch: local {SCHEMA_VERSION} vs peer {}",
                peer.schema_version
            )));
        }
        // Same MAJOR is required.
        if !self.version.same_major(&peer.protocol_version) {
            return Err(TransportError::IncompatibleVersion(format!(
                "major mismatch: local {} vs peer {}",
                self.version, peer.protocol_version
            )));
        }
        // Peer must be at least our minimum.
        if peer.protocol_version < self.min_supported {
            return Err(TransportError::IncompatibleVersion(format!(
                "peer {} below our min_supported {}",
                peer.protocol_version, self.min_supported
            )));
        }
        // We must be at least the peer's minimum (they would reject us otherwise).
        if self.version < peer.min_supported {
            return Err(TransportError::IncompatibleVersion(format!(
                "local {} below peer min_supported {}",
                self.version, peer.min_supported
            )));
        }
        // Optional engine-version matching for quorum determinism.
        if self.require_matching_engine
            && !self.engine_version.is_empty()
            && peer.engine_version != self.engine_version
        {
            return Err(TransportError::IncompatibleVersion(format!(
                "engine version mismatch: local {:?} vs peer {:?}",
                self.engine_version, peer.engine_version
            )));
        }
        // Newer side downgrades: negotiated = min of the two versions.
        let negotiated_version = self.version.min(peer.protocol_version);
        Ok(Negotiated {
            version: negotiated_version,
            peer_engine_version: peer.engine_version.clone(),
            peer_extension_version: peer.extension_version.clone(),
        })
    }
}

/// The result of a successful handshake negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// The common protocol version both sides will use.
    pub version: Version,
    pub peer_engine_version: String,
    pub peer_extension_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: Version, min: Version) -> VersionInfo {
        VersionInfo {
            version,
            min_supported: min,
            engine_version: "duckdb-1.1".into(),
            extension_version: "0.1.0".into(),
            require_matching_engine: false,
        }
    }

    fn hello(version: Version, min: Version, engine: &str) -> Hello {
        Hello {
            schema_version: SCHEMA_VERSION,
            protocol_version: version,
            min_supported: min,
            node_id: NodeId("b3:x".into()),
            engine_version: engine.into(),
            extension_version: "0.1.0".into(),
        }
    }

    #[test]
    fn compatible_minor_difference_negotiates_lower() {
        let local = info(Version::new(1, 3, 0), Version::new(1, 0, 0));
        let peer = hello(Version::new(1, 1, 0), Version::new(1, 0, 0), "duckdb-1.1");
        let n = local.negotiate(&peer).unwrap();
        assert_eq!(n.version, Version::new(1, 1, 0));
    }

    #[test]
    fn major_mismatch_rejected() {
        let local = info(Version::new(1, 0, 0), Version::new(1, 0, 0));
        let peer = hello(Version::new(2, 0, 0), Version::new(2, 0, 0), "e");
        assert!(matches!(
            local.negotiate(&peer),
            Err(TransportError::IncompatibleVersion(_))
        ));
    }

    #[test]
    fn peer_below_min_supported_rejected() {
        let local = info(Version::new(1, 5, 0), Version::new(1, 4, 0));
        let peer = hello(Version::new(1, 2, 0), Version::new(1, 0, 0), "e");
        assert!(matches!(
            local.negotiate(&peer),
            Err(TransportError::IncompatibleVersion(_))
        ));
    }

    #[test]
    fn local_below_peer_min_supported_rejected() {
        let local = info(Version::new(1, 1, 0), Version::new(1, 0, 0));
        let peer = hello(Version::new(1, 5, 0), Version::new(1, 4, 0), "e");
        assert!(matches!(
            local.negotiate(&peer),
            Err(TransportError::IncompatibleVersion(_))
        ));
    }

    #[test]
    fn engine_mismatch_rejected_when_required() {
        let mut local = info(Version::new(1, 0, 0), Version::new(1, 0, 0));
        local.require_matching_engine = true;
        let peer = hello(Version::new(1, 0, 0), Version::new(1, 0, 0), "duckdb-9.9");
        assert!(matches!(
            local.negotiate(&peer),
            Err(TransportError::IncompatibleVersion(_))
        ));
    }
}
