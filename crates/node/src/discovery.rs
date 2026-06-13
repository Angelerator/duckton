//! Peer discovery (architecture §8).
//!
//! [`Discovery`] is pluggable; the MVP [`StaticDiscovery`] uses a static seed
//! list. Crucially, discovery returns a **bounded candidate sample** rather than
//! every peer — this is what keeps a requester's fan-out sub-linear as the swarm
//! grows to thousands of hosts. The Kademlia + gossip implementation (Phase 3)
//! slots in behind this same trait.

use std::net::SocketAddr;
use std::sync::Mutex;

use async_trait::async_trait;
use p2p_proto::{AttestationLevel, DataClass, NodeId};
use rand::seq::SliceRandom;

/// A discovered worker candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Known node id, if any (enables pinning; `None` => trust-on-first-use).
    pub node_id: Option<NodeId>,
    pub addr: SocketAddr,
    /// Advertised attestation level (from gossip capability ad), if known.
    pub advertised_level: Option<AttestationLevel>,
}

impl Candidate {
    pub fn new(node_id: Option<NodeId>, addr: SocketAddr) -> Self {
        Self {
            node_id,
            addr,
            advertised_level: None,
        }
    }
}

/// A filter describing what kind of candidates a requester wants.
#[derive(Debug, Clone, Copy)]
pub struct CandidateFilter {
    pub data_class: DataClass,
    pub min_attestation: AttestationLevel,
}

/// Pluggable discovery.
#[async_trait]
pub trait Discovery: Send + Sync {
    /// Return up to `want` candidate workers matching `filter`. Implementations
    /// MUST bound the returned set (never the whole swarm).
    async fn find_candidates(&self, want: usize, filter: CandidateFilter) -> Vec<Candidate>;
}

/// Static seed-list discovery with bounded random sampling.
pub struct StaticDiscovery {
    peers: Vec<Candidate>,
    /// Hard cap on how many candidates are ever returned, regardless of `want`.
    sample_cap: usize,
    rng_seed: Mutex<u64>,
}

impl StaticDiscovery {
    pub fn new(peers: Vec<Candidate>, sample_cap: usize) -> Self {
        Self {
            peers,
            sample_cap: sample_cap.max(1),
            rng_seed: Mutex::new(0x9e3779b97f4a7c15),
        }
    }

    fn next_rng(&self) -> rand::rngs::StdRng {
        use rand::SeedableRng;
        let mut seed = self.rng_seed.lock().unwrap();
        // simple xorshift to vary sampling between calls
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        rand::rngs::StdRng::seed_from_u64(*seed)
    }
}

#[async_trait]
impl Discovery for StaticDiscovery {
    async fn find_candidates(&self, want: usize, filter: CandidateFilter) -> Vec<Candidate> {
        let take = want.min(self.sample_cap);
        // Filter by advertised attestation when known; unknown levels are kept
        // (verified later during bidding).
        let mut eligible: Vec<Candidate> = self
            .peers
            .iter()
            .filter(|c| match c.advertised_level {
                Some(level) => level >= filter.min_attestation,
                None => true,
            })
            .cloned()
            .collect();
        let mut rng = self.next_rng();
        eligible.shuffle(&mut rng);
        eligible.truncate(take);
        eligible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(port: u16) -> Candidate {
        Candidate::new(None, format!("127.0.0.1:{port}").parse().unwrap())
    }

    fn filter() -> CandidateFilter {
        CandidateFilter {
            data_class: DataClass::Public,
            min_attestation: AttestationLevel::L0,
        }
    }

    #[tokio::test]
    async fn returns_bounded_sample() {
        let peers: Vec<_> = (0..1000).map(|i| peer(10_000 + i)).collect();
        let disc = StaticDiscovery::new(peers, 16);
        // even asking for 1000, we never exceed the sample cap
        let got = disc.find_candidates(1000, filter()).await;
        assert_eq!(got.len(), 16);
    }

    #[tokio::test]
    async fn respects_want_below_cap() {
        let peers: Vec<_> = (0..100).map(|i| peer(20_000 + i)).collect();
        let disc = StaticDiscovery::new(peers, 16);
        let got = disc.find_candidates(5, filter()).await;
        assert_eq!(got.len(), 5);
    }

    #[tokio::test]
    async fn filters_by_advertised_attestation() {
        let mut p = peer(30_000);
        p.advertised_level = Some(AttestationLevel::L0);
        let disc = StaticDiscovery::new(vec![p], 16);
        let f = CandidateFilter {
            data_class: DataClass::Sensitive,
            min_attestation: AttestationLevel::L2,
        };
        assert!(disc.find_candidates(10, f).await.is_empty());
    }
}
