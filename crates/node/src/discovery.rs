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
    /// Advertised logical partitions (from the capability ad). Empty ⇒ unknown
    /// (the host re-checks at admission) or the implicit `"default"` partition.
    pub advertised_networks: Vec<String>,
    /// Advertised group memberships (from the capability ad). Empty ⇒ ungrouped /
    /// unknown.
    pub advertised_groups: Vec<String>,
    /// Advertised region (from the capability ad). `None` ⇒ unknown.
    pub advertised_region: Option<String>,
}

impl Candidate {
    pub fn new(node_id: Option<NodeId>, addr: SocketAddr) -> Self {
        Self {
            node_id,
            addr,
            advertised_level: None,
            advertised_networks: Vec::new(),
            advertised_groups: Vec::new(),
            advertised_region: None,
        }
    }
}

/// A filter describing what kind of candidates a requester wants.
#[derive(Debug, Clone)]
pub struct CandidateFilter {
    pub data_class: DataClass,
    pub min_attestation: AttestationLevel,
    /// Target logical partition (`None` ⇒ any). Soft prune: an ad with KNOWN
    /// networks not including this is dropped; an unlabeled ad is kept.
    pub network: Option<String>,
    /// The requester's group claims. Soft prune: an ad advertising groups must
    /// share ≥1; an ungrouped/unknown ad is kept.
    pub groups: Vec<String>,
    /// Accepted regions (empty ⇒ no constraint). Fail-closed prune: only an ad
    /// whose region is in the set is kept.
    pub regions: Vec<String>,
}

impl CandidateFilter {
    /// Whether a candidate's ADVERTISED request-scoping labels pass this filter
    /// (architecture §7.5). Network/group are SOFT (a candidate with unknown —
    /// unadvertised — labels is kept; only a KNOWN, non-matching label drops it);
    /// region is FAIL-CLOSED (a region-pinned query keeps only a candidate whose
    /// advertised region is in the set). All-empty filter ⇒ always `true`. Shared
    /// by the discovery prune (early optimization) and the coordinator retain
    /// (enforcement), so they can't disagree.
    pub fn admits_labels(&self, c: &Candidate) -> bool {
        if let Some(net) = &self.network {
            if !c.advertised_networks.is_empty()
                && !c.advertised_networks.iter().any(|n| n == net)
            {
                return false;
            }
        }
        if !c.advertised_groups.is_empty()
            && !c.advertised_groups.iter().any(|g| self.groups.contains(g))
        {
            return false;
        }
        if !self.regions.is_empty() {
            match &c.advertised_region {
                Some(r) if self.regions.iter().any(|x| x == r) => {}
                _ => return false,
            }
        }
        true
    }
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
            // Request-scoping prune by advertised labels (network/group soft,
            // region fail-closed) — an early optimization; the coordinator retain
            // re-checks. No-op when the filter sets no constraint.
            .filter(|c| filter.admits_labels(c))
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
            network: None,
            groups: vec![],
            regions: vec![],
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
            network: None,
            groups: vec![],
            regions: vec![],
        };
        assert!(disc.find_candidates(10, f).await.is_empty());
    }

    #[test]
    fn admits_labels_network_group_soft_region_failclosed() {
        // The shared matcher used by both the discovery prune and the coordinator
        // retain (§7.5). Network/group are SOFT (unknown labels are kept; only a
        // KNOWN, non-matching label drops); region is FAIL-CLOSED (unknown region
        // dropped). Mirrors the `require_staked_hosts` fail-closed shape. Each
        // dimension is tested on a single-labeled candidate so they don't conflate.
        let mut net = peer(40_100);
        net.advertised_networks = vec!["eu".into()];
        let mut grp = peer(40_101);
        grp.advertised_groups = vec!["finance".into()];
        let mut reg = peer(40_102);
        reg.advertised_region = Some("eu".into());
        let unknown = peer(40_103); // no advertised labels

        let f = |network: Option<&str>, groups: Vec<&str>, regions: Vec<&str>| CandidateFilter {
            data_class: DataClass::Public,
            min_attestation: AttestationLevel::L0,
            network: network.map(String::from),
            groups: groups.into_iter().map(String::from).collect(),
            regions: regions.into_iter().map(String::from).collect(),
        };

        // No constraint ⇒ unlabeled / network-only / region-only candidates kept.
        assert!(f(None, vec![], vec![]).admits_labels(&net));
        assert!(f(None, vec![], vec![]).admits_labels(&unknown));

        // Network (soft): known-but-wrong dropped; matching kept; unknown kept.
        assert!(!f(Some("us"), vec![], vec![]).admits_labels(&net));
        assert!(f(Some("eu"), vec![], vec![]).admits_labels(&net));
        assert!(f(Some("us"), vec![], vec![]).admits_labels(&unknown));

        // Group: a grouped host is a PRIVATE pool — reachable only by a requester
        // sharing a group; an ungrouped requester is pruned. An unknown/ungrouped
        // candidate is kept (soft).
        assert!(!f(None, vec!["ops"], vec![]).admits_labels(&grp));
        assert!(f(None, vec!["finance"], vec![]).admits_labels(&grp));
        assert!(!f(None, vec![], vec![]).admits_labels(&grp));
        assert!(f(None, vec!["ops"], vec![]).admits_labels(&unknown));

        // Region (fail-closed): only an advertised region in the set is kept; an
        // unknown region is always dropped.
        assert!(!f(None, vec![], vec!["us"]).admits_labels(&reg));
        assert!(f(None, vec![], vec!["eu"]).admits_labels(&reg));
        assert!(!f(None, vec![], vec!["eu"]).admits_labels(&unknown));
    }
}
