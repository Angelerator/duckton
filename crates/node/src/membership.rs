//! Scalable membership view (architecture §8).
//!
//! A node maintains a **bounded** local cache of verified capability ads — its
//! window into the gossip/DHT layer. Requesters sample a bounded candidate set
//! from this cache; they never enumerate the whole swarm, so fan-out stays
//! sub-linear as the network grows to thousands of hosts.
//!
//! This implements the [`Discovery`] trait, so it is a drop-in replacement for
//! [`crate::StaticDiscovery`]. The wire-level propagation of ads (libp2p
//! Kademlia + gossipsub) is the production transport that feeds [`ingest`]; that
//! networking is future work, but the local membership/sampling/verification
//! logic — the part that determines scalability — is implemented and tested here.
//!
//! [`ingest`]: MembershipTable::ingest

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use p2p_proto::{CapabilityAd, CapabilityProfile, NodeId};
use p2p_trust::{now_ts, verify_capability_ad, verify_capability_profile};
use rand::seq::SliceRandom;

use crate::discovery::{Candidate, CandidateFilter, Discovery};

/// A bounded, LRU-evicted table of verified capability ads.
pub struct MembershipTable {
    inner: Mutex<Inner>,
    capacity: usize,
    required_pow_bits: u32,
    capability_ttl_secs: u64,
}

struct Inner {
    ads: HashMap<NodeId, CapabilityAd>,
    /// Per-node durable self-capability profiles (the "what this node has really
    /// pulled off" hint). Kept ONLY as a capacity/routing signal — never folded
    /// into trust/reputation, since the maxima are self-claimed (signing proves
    /// provenance + monotonicity, not honesty).
    profiles: HashMap<NodeId, CapabilityProfile>,
    order: VecDeque<NodeId>,
}

impl MembershipTable {
    pub fn new(capacity: usize, required_pow_bits: u32, capability_ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                ads: HashMap::new(),
                profiles: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity: capacity.max(1),
            required_pow_bits,
            capability_ttl_secs,
        }
    }

    /// Ingest a capability ad received from the gossip/DHT layer. Returns `true`
    /// if it verified and was stored. Rejects bad signatures / insufficient PoW.
    pub fn ingest(&self, ad: CapabilityAd) -> bool {
        if !verify_capability_ad(&ad, self.required_pow_bits) {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        let key = ad.node_id.clone();
        if !inner.ads.contains_key(&key) {
            while inner.order.len() >= self.capacity {
                if let Some(evict) = inner.order.pop_front() {
                    inner.ads.remove(&evict);
                    inner.profiles.remove(&evict);
                } else {
                    break;
                }
            }
            inner.order.push_back(key.clone());
        }
        inner.ads.insert(key, ad);
        true
    }

    /// Ingest a node's signed [`CapabilityProfile`] (the durable self-measured
    /// maxima) gossiped alongside its [`CapabilityAd`]. Returns `true` if accepted.
    ///
    /// TRUST-SAFE BY CONSTRUCTION: the profile is stored ONLY as a capacity/routing
    /// hint (read via [`MembershipTable::proven_capacity`]) and is NEVER fed into
    /// trust/reputation — the maxima are self-claimed, so the signature proves only
    /// provenance + node-id binding, not honesty. Acceptance requires:
    ///   * a valid signature + node-id↔pubkey binding (`verify_capability_profile`),
    ///   * the node already has a **PoW-verified ad** in the table (so a profile
    ///     can't be injected for a fresh, un-PoW'd identity — it inherits the ad's
    ///     Sybil cost), and
    ///   * a **strictly increasing `seq`** vs any stored profile (rollback guard).
    pub fn ingest_profile(&self, profile: CapabilityProfile) -> bool {
        if !verify_capability_profile(&profile) {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        // PoW gate (by proxy): only accept a profile for an identity that already
        // proved PoW via a verified ad in this table.
        if !inner.ads.contains_key(&profile.node_id) {
            return false;
        }
        // Monotonic seq: never replace a newer snapshot with an older one.
        if let Some(prev) = inner.profiles.get(&profile.node_id) {
            if profile.seq <= prev.seq {
                return false;
            }
        }
        inner.profiles.insert(profile.node_id.clone(), profile);
        true
    }

    /// The latest verified self-capability profile for `node`, if one was gossiped
    /// — a capacity/routing hint ("can this peer plausibly handle a job of size
    /// X?"). NEVER a trust input (see [`MembershipTable::ingest_profile`]).
    pub fn proven_capacity(&self, node: &NodeId) -> Option<CapabilityProfile> {
        self.inner.lock().unwrap().profiles.get(node).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().ads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn fresh(&self, ad: &CapabilityAd, now: u64) -> bool {
        now.saturating_sub(ad.ts) <= self.capability_ttl_secs
    }
}

#[async_trait]
impl Discovery for MembershipTable {
    async fn find_candidates(&self, want: usize, filter: CandidateFilter) -> Vec<Candidate> {
        let now = now_ts();
        let inner = self.inner.lock().unwrap();
        let mut eligible: Vec<Candidate> = inner
            .ads
            .values()
            // A host on standby (graceful drain) advertises `enabled = false`;
            // skip it (it declines new offers anyway).
            .filter(|ad| ad.enabled)
            .filter(|ad| self.fresh(ad, now))
            .filter(|ad| ad.attestation_level >= filter.min_attestation)
            .filter_map(|ad| {
                let addr = ad.addr.parse().ok()?;
                // The id is bound by the ad's signature + node-id binding + PoW
                // (verified on ingest), so it is `Advertised` (gate-eligible).
                let mut c = Candidate::new(Some(ad.node_id.clone()), addr).advertised();
                c.advertised_level = Some(ad.attestation_level);
                // Carry the advertised request-scoping labels so the filter can
                // prune by them (and the coordinator can re-check).
                c.advertised_networks = ad.networks.clone();
                c.advertised_groups = ad.groups.clone();
                c.advertised_region = ad.region.clone();
                Some(c)
            })
            // Request-scoping prune by advertised labels (network/group soft,
            // region fail-closed).
            .filter(|c| filter.admits_labels(c))
            .collect();
        drop(inner);

        let take = want.min(self.capacity);
        let mut rng = rand::thread_rng();
        eligible.shuffle(&mut rng);
        eligible.truncate(take);
        eligible
    }

    fn proven_capacity(&self, id: &NodeId) -> Option<CapabilityProfile> {
        // Inherent method (priority over this trait method); no recursion.
        MembershipTable::proven_capacity(self, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_proto::AttestationLevel;
    use p2p_transport::NodeIdentity;
    use p2p_trust::sybil::pow_epoch;
    use p2p_trust::{mint_pow, sign_capability_ad, CapabilityDraft};

    use crate::signer::IdentitySigner;

    fn signed_ad(port: u16, ts: u64) -> CapabilityAd {
        let id = NodeIdentity::generate().unwrap();
        let pk = id.public_key_bytes();
        let draft = CapabilityDraft {
            addr: format!("127.0.0.1:{port}"),
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            max_jobs: 3,
            attestation_level: AttestationLevel::L0,
            price: 0,
            recent_receipts_root: None,
            // PoW epoch MUST match `pow_epoch(ad.ts)` or `verify_capability_ad`
            // (which derives the epoch from `ad.ts`) rejects the ad.
            pow: mint_pow(&pk, pow_epoch(ts), 8, 1_000_000).unwrap(),
            ts,
            enabled: true,
            networks: vec!["default".into()],
            groups: vec![],
            region: None,
        };
        sign_capability_ad(draft, &IdentitySigner(&id))
    }

    fn labeled_ad(
        port: u16,
        ts: u64,
        enabled: bool,
        networks: Vec<&str>,
        groups: Vec<&str>,
        region: Option<&str>,
    ) -> CapabilityAd {
        let id = NodeIdentity::generate().unwrap();
        let pk = id.public_key_bytes();
        let draft = CapabilityDraft {
            addr: format!("127.0.0.1:{port}"),
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            max_jobs: 3,
            attestation_level: AttestationLevel::L0,
            price: 0,
            recent_receipts_root: None,
            pow: mint_pow(&pk, pow_epoch(ts), 8, 1_000_000).unwrap(),
            ts,
            enabled,
            networks: networks.into_iter().map(String::from).collect(),
            groups: groups.into_iter().map(String::from).collect(),
            region: region.map(String::from),
        };
        sign_capability_ad(draft, &IdentitySigner(&id))
    }

    fn filter() -> CandidateFilter {
        CandidateFilter {
            data_class: p2p_proto::DataClass::Public,
            min_attestation: AttestationLevel::L0,
            network: None,
            groups: vec![],
            regions: vec![],
            fail_closed_labels: false,
        }
    }

    #[tokio::test]
    async fn find_candidates_prunes_standby_network_and_region() {
        let table = MembershipTable::new(50, 8, 3600);
        let now = now_ts();
        // A serving, ungrouped EU host.
        assert!(table.ingest(labeled_ad(41_000, now, true, vec!["eu"], vec![], Some("eu"))));
        // A standby host (advertises enabled=false) — must never be a candidate.
        assert!(table.ingest(labeled_ad(41_001, now, false, vec!["eu"], vec![], Some("eu"))));

        // Standby is pruned even with no constraint; the serving host remains.
        assert_eq!(
            table.find_candidates(10, filter()).await.len(),
            1,
            "standby ad must be excluded"
        );

        // Network prune: targeting "us" drops the EU host (known, non-matching).
        let mut us = filter();
        us.network = Some("us".into());
        assert!(table.find_candidates(10, us).await.is_empty());

        // Region fail-closed: a ["us"] pin drops the EU host; an ["eu"] pin keeps it.
        let mut us_region = filter();
        us_region.regions = vec!["us".into()];
        assert!(table.find_candidates(10, us_region).await.is_empty());
        let mut eu_region = filter();
        eu_region.regions = vec!["eu".into()];
        assert_eq!(table.find_candidates(10, eu_region).await.len(), 1);
    }

    #[tokio::test]
    async fn find_candidates_group_is_a_private_pool() {
        let table = MembershipTable::new(50, 8, 3600);
        let now = now_ts();
        // A grouped (finance) host + an ungrouped public host.
        assert!(table.ingest(labeled_ad(42_000, now, true, vec!["default"], vec!["finance"], None)));
        assert!(table.ingest(labeled_ad(42_001, now, true, vec!["default"], vec![], None)));

        // An ungrouped requester reaches only the public host (groups are private).
        assert_eq!(table.find_candidates(10, filter()).await.len(), 1);
        // A finance requester reaches both (its claim matches the grouped host).
        let mut fin = filter();
        fin.groups = vec!["finance".into()];
        assert_eq!(table.find_candidates(10, fin).await.len(), 2);
    }

    #[tokio::test]
    async fn ingest_verifies_and_samples_bounded() {
        let table = MembershipTable::new(50, 8, 3600);
        let now = now_ts();
        for p in 0..200u16 {
            assert!(table.ingest(signed_ad(10_000 + p, now)));
        }
        // bounded capacity
        assert_eq!(table.len(), 50);
        // bounded candidate sample
        let cands = table.find_candidates(1000, filter()).await;
        assert!(cands.len() <= 50);
        assert!(!cands.is_empty());
    }

    #[tokio::test]
    async fn rejects_unverifiable_ad() {
        let table = MembershipTable::new(50, 8, 3600);
        let mut ad = signed_ad(10_000, now_ts());
        ad.free_threads = 9999; // tamper
        assert!(!table.ingest(ad));
        assert_eq!(table.len(), 0);
    }

    #[tokio::test]
    async fn stale_ads_excluded_from_candidates() {
        let table = MembershipTable::new(50, 8, 30);
        let now = now_ts();
        // ts far in the past => stale
        assert!(table.ingest(signed_ad(10_000, now.saturating_sub(10_000))));
        assert_eq!(table.len(), 1);
        let cands = table.find_candidates(10, filter()).await;
        assert!(cands.is_empty(), "stale ad must not be a candidate");
    }

    #[tokio::test]
    async fn ingest_profile_requires_pow_verified_ad_and_is_monotonic() {
        use p2p_trust::{sign_capability_profile, CapabilityProfileDraft};

        let table = MembershipTable::new(50, 8, 3600);
        let id = NodeIdentity::generate().unwrap();
        let pk = id.public_key_bytes();
        let now = now_ts();
        let signer = IdentitySigner(&id);
        let nid = id.node_id().clone();

        let mk = |seq: u64, rows: u64| {
            sign_capability_profile(
                CapabilityProfileDraft {
                    max_result_rows: rows,
                    successes: seq,
                    seq,
                    ts: now,
                    ..Default::default()
                },
                &signer,
            )
        };

        // No verified ad yet ⇒ profile refused (PoW-by-proxy gate), nothing stored.
        assert!(!table.ingest_profile(mk(1, 100)));
        assert!(table.proven_capacity(&nid).is_none());

        // Ingest the node's PoW-verified ad, then the profile is accepted.
        let draft = CapabilityDraft {
            addr: "127.0.0.1:19000".into(),
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            max_jobs: 3,
            attestation_level: AttestationLevel::L0,
            price: 0,
            recent_receipts_root: None,
            pow: mint_pow(&pk, pow_epoch(now), 8, 1_000_000).unwrap(),
            ts: now,
            enabled: true,
            networks: vec!["default".into()],
            groups: vec![],
            region: None,
        };
        assert!(table.ingest(sign_capability_ad(draft, &signer)));
        assert!(table.ingest_profile(mk(2, 100)));
        assert_eq!(table.proven_capacity(&nid).unwrap().max_result_rows, 100);

        // Monotonic seq: equal/older seq rejected; strictly newer updates.
        assert!(!table.ingest_profile(mk(2, 999)));
        assert!(!table.ingest_profile(mk(1, 999)));
        assert!(table.ingest_profile(mk(3, 500)));
        assert_eq!(table.proven_capacity(&nid).unwrap().max_result_rows, 500);

        // A tampered profile (signature mismatch) is rejected.
        let mut bad = mk(4, 1);
        bad.max_result_rows = u64::MAX;
        assert!(!table.ingest_profile(bad));
    }
}
