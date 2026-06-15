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
use p2p_proto::{CapabilityAd, NodeId};
use p2p_trust::{now_ts, verify_capability_ad};
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
    order: VecDeque<NodeId>,
}

impl MembershipTable {
    pub fn new(capacity: usize, required_pow_bits: u32, capability_ttl_secs: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                ads: HashMap::new(),
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
                } else {
                    break;
                }
            }
            inner.order.push_back(key.clone());
        }
        inner.ads.insert(key, ad);
        true
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
            .filter(|ad| self.fresh(ad, now))
            .filter(|ad| ad.attestation_level >= filter.min_attestation)
            .filter_map(|ad| {
                let addr = ad.addr.parse().ok()?;
                let mut c = Candidate::new(Some(ad.node_id.clone()), addr);
                c.advertised_level = Some(ad.attestation_level);
                Some(c)
            })
            .collect();
        drop(inner);

        let take = want.min(self.capacity);
        let mut rng = rand::thread_rng();
        eligible.shuffle(&mut rng);
        eligible.truncate(take);
        eligible
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
        };
        sign_capability_ad(draft, &IdentitySigner(&id))
    }

    fn filter() -> CandidateFilter {
        CandidateFilter {
            data_class: p2p_proto::DataClass::Public,
            min_attestation: AttestationLevel::L0,
        }
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
}
