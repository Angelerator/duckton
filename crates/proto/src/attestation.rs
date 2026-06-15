//! Attestation model.
//!
//! Phase 0 only needs a *stub* attestation field in the bid (per roadmap §13).
//! We define the full tier enum up front (L0/L1/L2) and an `Attestation`
//! envelope carrying opaque evidence bytes; the real evidence formats and
//! verification live in `p2p-trust` (Phase 4). Keeping the type here lets the
//! proto messages reference it from Phase 0 onward.

use serde::{Deserialize, Serialize};

/// Attestation tier a worker claims. See architecture §7.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AttestationLevel {
    /// Anonymous: only the pinned node key (identity continuity).
    L0,
    /// Measured boot: TPM quote + signed event log.
    L1,
    /// Confidential TEE: hardware attestation quote against an allowlisted
    /// enclave measurement (Intel TDX / AMD SEV-SNP / AWS Nitro).
    L2,
}

impl AttestationLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            AttestationLevel::L0 => "L0",
            AttestationLevel::L1 => "L1",
            AttestationLevel::L2 => "L2",
        }
    }
}

impl std::str::FromStr for AttestationLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "L0" => Ok(AttestationLevel::L0),
            "L1" => Ok(AttestationLevel::L1),
            "L2" => Ok(AttestationLevel::L2),
            other => Err(format!("unknown attestation level: {other}")),
        }
    }
}

/// An attestation envelope advertised in a [`crate::Bid`].
///
/// In Phase 0 this is a stub: `level = L0` and empty evidence. In Phase 4 the
/// `evidence` carries a serialized attestation quote which `p2p-trust` verifies
/// against an allowlist of enclave measurements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub level: AttestationLevel,
    /// Opaque, tier-specific evidence (TPM quote / TEE report). Empty for L0.
    #[serde(with = "hex_bytes")]
    pub evidence: Vec<u8>,
    /// Optional identifier of the enclave/image measurement being claimed.
    pub measurement: Option<String>,
}

impl Attestation {
    /// The Phase-0 stub: anonymous, no evidence.
    pub fn stub_l0() -> Self {
        Self {
            level: AttestationLevel::L0,
            evidence: Vec::new(),
            measurement: None,
        }
    }
}

impl Default for Attestation {
    fn default() -> Self {
        Self::stub_l0()
    }
}

/// Serialize `Vec<u8>` as a hex string for readable JSON frames.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn level_ordering_is_l0_lt_l1_lt_l2() {
        assert!(AttestationLevel::L0 < AttestationLevel::L1);
        assert!(AttestationLevel::L1 < AttestationLevel::L2);
    }

    #[test]
    fn level_parses_case_insensitively() {
        assert_eq!(
            AttestationLevel::from_str("l2").unwrap(),
            AttestationLevel::L2
        );
        assert!(AttestationLevel::from_str("L9").is_err());
    }

    #[test]
    fn stub_is_l0_empty() {
        let a = Attestation::stub_l0();
        assert_eq!(a.level, AttestationLevel::L0);
        assert!(a.evidence.is_empty());
        let bytes = crate::to_bytes(&a).unwrap();
        let back: Attestation = crate::from_bytes(&bytes).unwrap();
        assert_eq!(a, back);
    }
}
