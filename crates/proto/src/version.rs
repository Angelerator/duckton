//! Centralized protocol version constants + semver type (architecture §5.1).
//!
//! This is the single source of truth for the protocol name, the current
//! protocol version, the default minimum supported version, and the wire schema
//! tag. The ALPN identifier and the handshake `Hello` are derived from these, so
//! versioning is defined in exactly one place.
//!
//! Compatibility rules (enforced at the transport handshake):
//!  * **Same MAJOR required.** Different majors are incompatible and rejected.
//!    The ALPN (`duckdb-p2p/<major>`) makes cross-major peers fail to share a
//!    protocol at the TLS layer; the `Hello` exchange enforces it explicitly too.
//!  * **MINOR/PATCH may differ.** The newer peer downgrades to the negotiated
//!    common (lower) version. Unknown added fields are tolerated by the JSON
//!    wire form, so a minor addition doesn't break an older minor.
//!  * A peer **below `min_supported_version`** is rejected with a typed error.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Protocol name used in the ALPN identifier.
pub const PROTOCOL_NAME: &str = "duckdb-p2p";

/// The current protocol version this build implements.
pub const PROTOCOL_VERSION: Version = Version::new(1, 0, 0);

/// Default minimum protocol version this build will talk to. Configurable via
/// `[protocol].min_supported_version`.
pub const MIN_SUPPORTED_VERSION: Version = Version::new(1, 0, 0);

/// Wire schema tag prefixed to every framed message (defense-in-depth on top of
/// ALPN). Bumped only on breaking message-format changes; equals the protocol
/// MAJOR by construction.
pub const SCHEMA_VERSION: u16 = PROTOCOL_VERSION.major;

/// A semantic version (major.minor.patch). Ordering is lexicographic by
/// (major, minor, patch). Serialized as the string `"major.minor.patch"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl Version {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Whether two versions share a major (the hard compatibility requirement).
    pub fn same_major(&self, other: &Version) -> bool {
        self.major == other.major
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("expected major.minor.patch, got {s:?}"));
        }
        let parse = |p: &str, name: &str| -> Result<u16, String> {
            p.parse::<u16>()
                .map_err(|e| format!("bad {name} in version {s:?}: {e}"))
        };
        Ok(Version {
            major: parse(parts[0], "major")?,
            minor: parse(parts[1], "minor")?,
            patch: parse(parts[2], "patch")?,
        })
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Version::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// The ALPN identifier for a given protocol major version, e.g. `duckdb-p2p/1`.
pub fn alpn_for_major(major: u16) -> Vec<u8> {
    format!("{PROTOCOL_NAME}/{major}").into_bytes()
}

/// The ALPN identifier for the current protocol version.
pub fn current_alpn() -> Vec<u8> {
    alpn_for_major(PROTOCOL_VERSION.major)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_lexicographic() {
        assert!(Version::new(1, 0, 0) < Version::new(1, 1, 0));
        assert!(Version::new(1, 2, 0) < Version::new(2, 0, 0));
        assert!(Version::new(1, 0, 5) > Version::new(1, 0, 4));
    }

    #[test]
    fn string_roundtrip() {
        let v = Version::new(1, 2, 3);
        assert_eq!(v.to_string(), "1.2.3");
        assert_eq!(Version::from_str("1.2.3").unwrap(), v);
        assert!(Version::from_str("1.2").is_err());
        assert!(Version::from_str("a.b.c").is_err());
    }

    #[test]
    fn json_roundtrip_as_string() {
        let v = Version::new(3, 4, 5);
        let j = serde_json::to_string(&v).unwrap();
        assert_eq!(j, "\"3.4.5\"");
        let back: Version = serde_json::from_str(&j).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn alpn_encodes_major() {
        assert_eq!(alpn_for_major(1), b"duckdb-p2p/1".to_vec());
        assert_eq!(current_alpn(), b"duckdb-p2p/1".to_vec());
    }
}
