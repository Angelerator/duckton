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
use p2p_proto::{AttestationLevel, CapabilityProfile, DataClass, NodeId};
use rand::seq::SliceRandom;

/// How a candidate's `node_id` was learned — gates the fail-closed security
/// checks (`require_staked_hosts`, `sybil.min_stake`). Only a CRYPTOGRAPHICALLY
/// attributable id may satisfy those gates; a trust-on-first-use id (learned from
/// an unauthenticated source) must not, even once it has been pinned for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdProvenance {
    /// Operator-configured (static seed with an explicit id) — trusted by config.
    Pinned,
    /// From a signature + node-id-bound + PoW-verified capability ad.
    Advertised,
    /// Trust-on-first-use: no id, or an id learned without independent
    /// verification. Never satisfies a stake/sybil gate (fail closed).
    Tofu,
}

impl IdProvenance {
    /// Whether this id is attributable enough to satisfy the fail-closed
    /// stake/sybil gates (operator-pinned or signed-ad). TOFU is never verified.
    pub fn is_verified(self) -> bool {
        matches!(self, IdProvenance::Pinned | IdProvenance::Advertised)
    }
}

/// A discovered worker candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Known node id, if any (enables pinning; `None` => trust-on-first-use).
    pub node_id: Option<NodeId>,
    /// How `node_id` was learned (gates the stake/sybil fail-closed checks).
    pub id_provenance: IdProvenance,
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
        // An explicit id supplied at construction is operator-pinned (config seed);
        // no id is trust-on-first-use. Signed-ad candidates set `Advertised`
        // explicitly via [`Candidate::advertised`].
        let id_provenance = if node_id.is_some() {
            IdProvenance::Pinned
        } else {
            IdProvenance::Tofu
        };
        Self {
            node_id,
            id_provenance,
            addr,
            advertised_level: None,
            advertised_networks: Vec::new(),
            advertised_groups: Vec::new(),
            advertised_region: None,
        }
    }

    /// Mark this candidate's id as learned from a verified capability ad.
    pub fn advertised(mut self) -> Self {
        self.id_provenance = IdProvenance::Advertised;
        self
    }

    /// Pin a TOFU-learned id (e.g. from a handshake) for ROUTING/dedup WITHOUT
    /// granting it stake/sybil gate-eligibility — the id stays `Tofu`, so the
    /// fail-closed gates continue to exclude it. This is the safe way to remember
    /// an id we have not independently attested.
    pub fn with_tofu_id(mut self, node_id: NodeId) -> Self {
        self.node_id = Some(node_id);
        self.id_provenance = IdProvenance::Tofu;
        self
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
    /// Private/enterprise closure (`[security].mode = "private"`). When `true`,
    /// network and group labels become FAIL-CLOSED like region: a candidate whose
    /// label is UNKNOWN (unadvertised) is DROPPED when the requester has the
    /// corresponding constraint, instead of being kept on the soft assumption the
    /// host re-checks at admission. `false` (default / public) ⇒ today's soft
    /// behavior (unknown labels kept), byte-for-byte.
    pub fail_closed_labels: bool,
    /// Per-call **target node(s)** (`nodes => ['b3:...']`): when non-empty, ONLY
    /// candidates whose KNOWN `node_id` is in this set are admitted — applied
    /// inside discovery (so a targeted node is never randomly sampled out) AND in
    /// the coordinator retain. FAIL-CLOSED: a candidate with no known id (TOFU)
    /// can never be a target. Empty ⇒ no node constraint (unchanged routing).
    pub nodes: Vec<NodeId>,
}

impl CandidateFilter {
    /// Whether a candidate's ADVERTISED request-scoping labels pass this filter
    /// (architecture §7.5). In PUBLIC mode network/group are SOFT (a candidate
    /// with unknown — unadvertised — labels is kept; only a KNOWN, non-matching
    /// label drops it); region is always FAIL-CLOSED (a region-pinned query keeps
    /// only a candidate whose advertised region is in the set). In PRIVATE mode
    /// (`fail_closed_labels`) network/group also fail closed: an unknown label is
    /// dropped when the requester has that constraint. All-empty filter ⇒ always
    /// `true`. Shared by the discovery prune (early optimization) and the
    /// coordinator retain (enforcement), so they can't disagree.
    pub fn admits_labels(&self, c: &Candidate) -> bool {
        // Target-node selector (per-call `nodes => [...]`): when set, only the
        // exact requested ids are admitted. Fail-closed — a candidate with no
        // known id cannot be the target. Checked FIRST so a targeted query never
        // routes anywhere else, regardless of the other (label) constraints.
        if !self.nodes.is_empty() {
            match &c.node_id {
                Some(id) if self.nodes.contains(id) => {}
                _ => return false,
            }
        }
        if let Some(net) = &self.network {
            if c.advertised_networks.is_empty() {
                // Unknown network: kept (soft) in public; dropped (fail-closed) in
                // private — a closed grid never routes to an unlabeled candidate.
                if self.fail_closed_labels {
                    return false;
                }
            } else if !c.advertised_networks.iter().any(|n| n == net) {
                return false;
            }
        }
        if !c.advertised_groups.is_empty() {
            if !c.advertised_groups.iter().any(|g| self.groups.contains(g)) {
                return false;
            }
        } else if self.fail_closed_labels && !self.groups.is_empty() {
            // Unknown group + the requester has group constraints: dropped in
            // private mode (kept soft in public).
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

    /// Optional CAPACITY/ROUTING hint: the peer's gossiped, signed
    /// [`CapabilityProfile`] (self-measured maxima). NEVER a trust input — a
    /// requester may use it only to bias routing toward peers that can plausibly
    /// handle a job's size. Default `None` (impls without a profile cache return
    /// nothing, so selection is unchanged).
    fn proven_capacity(&self, _id: &NodeId) -> Option<CapabilityProfile> {
        None
    }
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
            fail_closed_labels: false,
            nodes: vec![],
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
    async fn node_targeting_restricts_to_exact_ids_fail_closed() {
        // Three identified peers + one TOFU (no-id) peer. A query targeting only
        // peer "b" must return EXACTLY peer "b" — never the others, and never the
        // id-less TOFU peer (fail-closed). This holds inside discovery, so the
        // target is reliably returned (not randomly sampled out) even with a small
        // sample cap and many peers.
        let with_id = |port: u16, id: &str| {
            Candidate::new(
                Some(NodeId(id.into())),
                format!("127.0.0.1:{port}").parse().unwrap(),
            )
        };
        let peers = vec![
            with_id(41_000, "b3:aaaa"),
            with_id(41_001, "b3:bbbb"),
            with_id(41_002, "b3:cccc"),
            peer(41_003), // TOFU, no id
        ];
        let disc = StaticDiscovery::new(peers, 16);

        let mut f = filter();
        f.nodes = vec![NodeId("b3:bbbb".into())];
        let got = disc.find_candidates(16, f).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].node_id, Some(NodeId("b3:bbbb".into())));

        // Targeting an id no candidate has ⇒ no candidates (no silent fallback).
        let mut f2 = filter();
        f2.nodes = vec![NodeId("b3:zzzz".into())];
        assert!(disc.find_candidates(16, f2).await.is_empty());

        // Targeting multiple ids returns exactly those that exist.
        let mut f3 = filter();
        f3.nodes = vec![NodeId("b3:aaaa".into()), NodeId("b3:cccc".into())];
        let got3 = disc.find_candidates(16, f3).await;
        assert_eq!(got3.len(), 2);
        assert!(got3
            .iter()
            .all(|c| matches!(&c.node_id, Some(id) if id.0 == "b3:aaaa" || id.0 == "b3:cccc")));
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
            fail_closed_labels: false,
            nodes: vec![],
        };
        assert!(disc.find_candidates(10, f).await.is_empty());
    }

    #[test]
    fn id_provenance_gates_stake_eligibility() {
        // A no-id seed is TOFU (never gate-eligible). An operator-supplied id is
        // Pinned (verified). A signed-ad id is Advertised (verified). A TOFU id
        // pinned for routing stays TOFU — so learning-then-pinning never relaxes
        // the fail-closed stake/sybil gates.
        let tofu = peer(50_000);
        assert_eq!(tofu.id_provenance, IdProvenance::Tofu);
        assert!(!tofu.id_provenance.is_verified());

        let addr = "127.0.0.1:50001".parse().unwrap();
        let pinned = Candidate::new(Some(NodeId("b3:op".into())), addr);
        assert_eq!(pinned.id_provenance, IdProvenance::Pinned);
        assert!(pinned.id_provenance.is_verified());

        let advertised = Candidate::new(Some(NodeId("b3:ad".into())), addr).advertised();
        assert_eq!(advertised.id_provenance, IdProvenance::Advertised);
        assert!(advertised.id_provenance.is_verified());

        // Pinning a TOFU-learned id keeps it routable but NOT gate-eligible.
        let learned = peer(50_002).with_tofu_id(NodeId("b3:learned".into()));
        assert_eq!(learned.node_id, Some(NodeId("b3:learned".into())));
        assert_eq!(learned.id_provenance, IdProvenance::Tofu);
        assert!(!learned.id_provenance.is_verified());
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
            fail_closed_labels: false,
            nodes: vec![],
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

    #[test]
    fn admits_labels_private_mode_drops_unknown_network_and_group() {
        // PRIVATE mode (`fail_closed_labels`): network/group join region in
        // failing closed — an UNKNOWN-labeled candidate is dropped when the
        // requester has that constraint, where public mode would keep it (soft).
        let net = peer(41_100); // no advertised labels at all
        let unknown = peer(41_101);

        let private = |network: Option<&str>, groups: Vec<&str>| CandidateFilter {
            data_class: DataClass::Public,
            min_attestation: AttestationLevel::L0,
            network: network.map(String::from),
            groups: groups.into_iter().map(String::from).collect(),
            regions: vec![],
            fail_closed_labels: true,
            nodes: vec![],
        };
        let public = |network: Option<&str>, groups: Vec<&str>| CandidateFilter {
            data_class: DataClass::Public,
            min_attestation: AttestationLevel::L0,
            network: network.map(String::from),
            groups: groups.into_iter().map(String::from).collect(),
            regions: vec![],
            fail_closed_labels: false,
            nodes: vec![],
        };

        // Unknown network: kept in public, dropped in private (when constrained).
        assert!(public(Some("acme"), vec![]).admits_labels(&net));
        assert!(!private(Some("acme"), vec![]).admits_labels(&net));

        // Unknown group + requester has group constraints: kept public, dropped
        // private.
        assert!(public(None, vec!["finance"]).admits_labels(&unknown));
        assert!(!private(None, vec!["finance"]).admits_labels(&unknown));

        // A KNOWN matching label is still admitted in private mode.
        let mut labeled = peer(41_102);
        labeled.advertised_networks = vec!["acme".into()];
        labeled.advertised_groups = vec!["finance".into()];
        assert!(private(Some("acme"), vec!["finance"]).admits_labels(&labeled));

        // No constraints ⇒ private mode keeps everything (closure only bites when
        // the requester actually scopes the query).
        assert!(private(None, vec![]).admits_labels(&unknown));
    }
}
