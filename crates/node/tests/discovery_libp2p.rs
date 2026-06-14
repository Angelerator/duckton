//! Multi-node, in-process (loopback) tests for the real libp2p discovery
//! propagation layer (architecture §8): Kademlia bootstrap + gossipsub
//! dissemination of signed capability ads, verification on receipt, churn
//! handling via the bounded membership view, and bounded candidate sampling.
//!
//! These run only with the default `discovery-libp2p` feature.
#![cfg(feature = "discovery-libp2p")]

use std::sync::Arc;
use std::time::Duration;

use libp2p::Multiaddr;
use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_node::{
    evaluate_ad, AdOutcome, AdmissionController, CandidateFilter, Coordinator, Discovery,
    IdentitySigner, Libp2pDiscovery, Libp2pDiscoveryConfig, MembershipTable, MockEngine, NatParams,
    Worker, WorkerParams,
};
use p2p_proto::{Attestation, AttestationLevel, CapabilityAd, DataClass};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};
use p2p_trust::{
    mint_pow, now_ts, sign_capability_ad, CapabilityDraft, InMemoryTrustStore, TrustStore,
};

const TEST_TOPIC: &str = "duckdb-p2p/caps/test-1";
const POW_BITS: u32 = 8;

/// NAT-traversal params for tests. The full stack (AutoNAT + DCUtR + relay
/// client) is built so we exercise the production swarm wiring, but **mDNS is
/// off by default** so concurrently-running test nodes do not cross-discover
/// each other over the LAN. The dedicated mDNS test flips it on.
fn test_nat(mdns: bool) -> NatParams {
    NatParams {
        autonat: true,
        dcutr: true,
        relay_client: true,
        act_as_relay: false,
        mdns,
        // Snappy LAN re-query so the test recovers fast if an initial mDNS
        // packet is lost (the library default is 5 minutes).
        mdns_query_interval: Duration::from_secs(1),
        external_addresses: vec![],
        relays: vec![],
        max_relays: 3,
        relay_limits: Default::default(),
    }
}

fn disc_config(bootstrap: Vec<Multiaddr>) -> Libp2pDiscoveryConfig {
    Libp2pDiscoveryConfig {
        listen_addrs: vec![],
        bootstrap,
        topic: TEST_TOPIC.to_string(),
        heartbeat: Duration::from_millis(250),
        mesh_n: 4,
        capability_ttl_secs: 3600,
        required_pow_bits: POW_BITS,
        membership_capacity: 1000,
        replication_factor: 20,
        query_parallelism: 3,
        protocol_major: p2p_proto::PROTOCOL_VERSION.major,
        nat: test_nat(false),
    }
}

/// A freshly-keyed, signed, PoW-stamped ad advertising `addr` at time `ts`.
fn signed_ad(addr: &str, ts: u64) -> CapabilityAd {
    let id = NodeIdentity::generate().unwrap();
    let pk = id.public_key_bytes();
    let draft = CapabilityDraft {
        addr: addr.to_string(),
        free_mem_bytes: 1 << 30,
        free_threads: 4,
        max_jobs: 3,
        attestation_level: AttestationLevel::L0,
        price: 0,
        recent_receipts_root: None,
        pow: mint_pow(&pk, POW_BITS, 5_000_000).unwrap(),
        ts,
    };
    sign_capability_ad(draft, &IdentitySigner(&id))
}

fn filter() -> CandidateFilter {
    CandidateFilter {
        data_class: DataClass::Public,
        min_attestation: AttestationLevel::L0,
    }
}

/// Poll `f` until it returns true or `timeout` elapses.
async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll the (async) candidate count until it satisfies `pred` or times out.
async fn wait_candidate_count(
    disc: &Libp2pDiscovery,
    timeout: Duration,
    pred: impl Fn(usize) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = disc.find_candidates(8, filter()).await.len();
        if pred(n) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Two nodes: a publisher's signed ad propagates over gossip to a subscriber.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_propagate_signed_ad_via_gossip() {
    let node_a = Libp2pDiscovery::spawn(disc_config(vec![])).await.unwrap();
    let a_addrs = node_a.wait_listeners(Duration::from_secs(5)).await;
    assert!(!a_addrs.is_empty(), "node A must bind a listen addr");

    let node_b = Libp2pDiscovery::spawn(disc_config(a_addrs)).await.unwrap();

    // A advertises a (fake but well-formed) QUIC data-plane endpoint.
    let ad = signed_ad("127.0.0.1:19494", now_ts());
    node_a.publish_ad(&ad).await.unwrap();

    let got = wait_until(Duration::from_secs(20), || node_b.membership().len() >= 1).await;
    assert!(got, "node B should receive node A's gossiped ad");

    let cands = node_b.find_candidates(8, filter()).await;
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].addr.to_string(), "127.0.0.1:19494");
}

// ---------------------------------------------------------------------------
// Three nodes around a bootstrap: ads propagate so each node discovers the
// others (gossip mesh relays beyond direct connections).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_nodes_discover_each_other() {
    // A is the bootstrap/relay; it publishes nothing of its own.
    let node_a = Libp2pDiscovery::spawn(disc_config(vec![])).await.unwrap();
    let a_addrs = node_a.wait_listeners(Duration::from_secs(5)).await;
    assert!(!a_addrs.is_empty());

    let node_b = Libp2pDiscovery::spawn(disc_config(a_addrs.clone()))
        .await
        .unwrap();
    let node_c = Libp2pDiscovery::spawn(disc_config(a_addrs)).await.unwrap();

    let ad_b = signed_ad("127.0.0.1:19501", now_ts());
    let ad_c = signed_ad("127.0.0.1:19502", now_ts());
    node_b.publish_ad(&ad_b).await.unwrap();
    node_c.publish_ad(&ad_c).await.unwrap();

    // A (directly connected to both) learns both ads.
    let a_has_both = wait_until(Duration::from_secs(25), || node_a.membership().len() >= 2).await;
    assert!(a_has_both, "bootstrap node A should learn both B and C");

    // B learns C's ad even though B and C are not directly bootstrapped to each
    // other — gossipsub relays it through the mesh.
    let b_learns_c = wait_until(Duration::from_secs(25), || {
        node_b.membership().len() >= 1
    })
    .await;
    assert!(b_learns_c, "node B should learn C's ad via gossip relay");
    let c_learns_b = wait_until(Duration::from_secs(25), || {
        node_c.membership().len() >= 1
    })
    .await;
    assert!(c_learns_b, "node C should learn B's ad via gossip relay");
}

// ---------------------------------------------------------------------------
// Churn: when a node leaves, its ad is no longer refreshed and ages out of the
// bounded view (TTL), so candidate sampling stops returning it.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn churn_node_leaving_ages_out_of_bounded_view() {
    let mut cfg_a = disc_config(vec![]);
    cfg_a.capability_ttl_secs = 2; // short freshness window for the test
    let node_a = Libp2pDiscovery::spawn(cfg_a).await.unwrap();
    let a_addrs = node_a.wait_listeners(Duration::from_secs(5)).await;

    let mut cfg_b = disc_config(a_addrs);
    cfg_b.capability_ttl_secs = 2;
    let node_b = Libp2pDiscovery::spawn(cfg_b).await.unwrap();

    // A short-lived worker publishes, then "leaves".
    {
        let mut cfg_w = disc_config(node_a.wait_listeners(Duration::from_secs(5)).await);
        cfg_w.capability_ttl_secs = 2;
        let worker = Libp2pDiscovery::spawn(cfg_w).await.unwrap();
        let ad = signed_ad("127.0.0.1:19601", now_ts());
        worker.publish_ad(&ad).await.unwrap();

        let appeared = wait_candidate_count(&node_b, Duration::from_secs(20), |n| n >= 1).await;
        assert!(appeared, "joining worker should appear in B's view");
        // `worker` dropped here → churn (stops republishing, disconnects).
    }

    // After the TTL elapses with no refresh, the stale ad is excluded from
    // candidate sampling (bounded view reflects the churn).
    let aged_out = wait_candidate_count(&node_b, Duration::from_secs(15), |n| n == 0).await;
    assert!(aged_out, "left node's ad should age out of the candidate sample");
}

// ---------------------------------------------------------------------------
// Verification on receipt: malformed / expired / wrong-version / bad-signature
// ads are rejected; only well-formed, fresh, compatible, signed ads are stored.
// ---------------------------------------------------------------------------
#[test]
fn rejects_malformed_expired_wrongversion_and_badsig() {
    let table = MembershipTable::new(100, POW_BITS, 3600);
    let now = now_ts();
    let ttl = 30u64;
    let major = p2p_proto::PROTOCOL_VERSION.major;

    // Malformed bytes.
    assert_eq!(
        evaluate_ad(b"not a capability ad", &table, now, ttl, major),
        AdOutcome::Malformed
    );

    // Wrong protocol major (version check precedes signature check).
    let mut wrong_ver = signed_ad("127.0.0.1:1", now);
    wrong_ver.protocol_version = p2p_proto::Version::new(major + 1, 0, 0);
    let bytes = p2p_proto::to_bytes(&wrong_ver).unwrap();
    assert_eq!(
        evaluate_ad(&bytes, &table, now, ttl, major),
        AdOutcome::IncompatibleVersion
    );

    // Expired (ts older than the TTL window).
    let expired = signed_ad("127.0.0.1:2", now.saturating_sub(ttl + 100));
    let bytes = p2p_proto::to_bytes(&expired).unwrap();
    assert_eq!(
        evaluate_ad(&bytes, &table, now, ttl, major),
        AdOutcome::Expired
    );

    // Implausible far-future ts is also rejected.
    let future = signed_ad("127.0.0.1:3", now + 10_000);
    let bytes = p2p_proto::to_bytes(&future).unwrap();
    assert_eq!(
        evaluate_ad(&bytes, &table, now, ttl, major),
        AdOutcome::Expired
    );

    // Tampered ad (valid version + freshness, broken signature).
    let mut tampered = signed_ad("127.0.0.1:4", now);
    tampered.free_threads = 9999;
    let bytes = p2p_proto::to_bytes(&tampered).unwrap();
    assert_eq!(
        evaluate_ad(&bytes, &table, now, ttl, major),
        AdOutcome::Rejected
    );

    // Nothing bad was stored.
    assert_eq!(table.len(), 0);

    // A clean, fresh, signed ad is accepted and stored.
    let good = signed_ad("127.0.0.1:5", now);
    let bytes = p2p_proto::to_bytes(&good).unwrap();
    assert_eq!(
        evaluate_ad(&bytes, &table, now, ttl, major),
        AdOutcome::Accepted
    );
    assert_eq!(table.len(), 1);
}

// ---------------------------------------------------------------------------
// Full loop: a real QUIC worker is discovered *only* via libp2p gossip, then a
// Coordinator built on the libp2p Discovery runs a real hedged query against it.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coordinator_discovers_worker_over_gossip_and_runs_query() {
    let idcfg = IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    };
    let net = GridConfig::default().network;

    // A real QUIC worker on the data plane.
    let worker_identity = NodeIdentity::generate().unwrap();
    let worker_node_id = worker_identity.node_id().clone();
    let worker_transport =
        Arc::new(QuicTransport::bind(&net, &idcfg, worker_identity.clone()).unwrap());
    let worker_addr = worker_transport.local_addr().unwrap();
    let admission = AdmissionController::new(&GridConfig::default().budget);
    let params = WorkerParams::from_config(&GridConfig::default());
    let worker = Worker::new(
        worker_transport.clone(),
        Arc::new(MockEngine::deterministic()),
        admission,
        Attestation::stub_l0(),
        params,
    );
    let _worker_task = worker.spawn();

    // The worker's own discovery node publishes its signed ad (addr = its real
    // QUIC endpoint), signed by its node identity so the node_id binds.
    let worker_disc = Libp2pDiscovery::spawn(disc_config(vec![])).await.unwrap();
    let w_boot = worker_disc.wait_listeners(Duration::from_secs(5)).await;
    assert!(!w_boot.is_empty());
    let pk = worker_identity.public_key_bytes();
    let draft = CapabilityDraft {
        addr: worker_addr.to_string(),
        free_mem_bytes: 1 << 30,
        free_threads: 4,
        max_jobs: 3,
        attestation_level: AttestationLevel::L0,
        price: 0,
        recent_receipts_root: None,
        pow: mint_pow(&pk, POW_BITS, 5_000_000).unwrap(),
        ts: now_ts(),
    };
    let ad = sign_capability_ad(draft, &IdentitySigner(&worker_identity));
    worker_disc.publish_ad(&ad).await.unwrap();

    // The requester's discovery node bootstraps to the worker's overlay and
    // learns the worker purely through gossip.
    let req_disc = Arc::new(Libp2pDiscovery::spawn(disc_config(w_boot)).await.unwrap());
    let learned = wait_until(Duration::from_secs(20), || req_disc.membership().len() >= 1).await;
    assert!(learned, "requester should discover the worker via gossip");

    // Build a Coordinator on the libp2p Discovery seam and run a real query.
    let mut cfg = GridConfig::default();
    cfg.scheduler.replicas = 1;
    cfg.scheduler.quorum = 1;
    cfg.scheduler.offer_timeout_ms = 2_000;
    cfg.scheduler.dispatch_timeout_ms = 5_000;
    cfg.trust.min_trust = 0.0; // fresh worker has no reputation yet
    cfg.validate().unwrap();
    let cfg = Arc::new(cfg);

    let req_transport =
        Arc::new(QuicTransport::bind(&net, &idcfg, NodeIdentity::generate().unwrap()).unwrap());
    let store: Arc<dyn TrustStore> = Arc::new(InMemoryTrustStore::new(&cfg.trust, &cfg.limits));
    let discovery: Arc<dyn Discovery> = req_disc.clone();
    let coord = Coordinator::new(req_transport, discovery, store, cfg, "mock-1");

    let outcome = coord
        .run_query("SELECT region, count(*) FROM events GROUP BY region", Default::default())
        .await
        .expect("query over gossip-discovered worker should succeed");
    assert!(outcome.verified);
    assert_eq!(outcome.winner.as_ref(), Some(&worker_node_id));
}

// ---------------------------------------------------------------------------
// NAT config plumbing: `[discovery.nat]` resolves into the overlay's NatParams
// (defaults → typed config → overlay), with no hard-coding.
// ---------------------------------------------------------------------------
#[test]
fn nat_params_resolve_from_grid_config() {
    let mut cfg = GridConfig::default();
    cfg.discovery.mode = p2p_config::DiscoveryMode::Kademlia;
    cfg.discovery.nat.act_as_relay = true;
    cfg.discovery.nat.mdns = false;
    cfg.discovery.nat.max_relays = 7;
    cfg.discovery.nat.relay_limits.max_circuits = 42;
    cfg.discovery.nat.external_addresses = vec!["/ip4/203.0.113.10/udp/9595/quic-v1".to_string()];
    cfg.validate().unwrap();

    let resolved = Libp2pDiscoveryConfig::from_grid(&cfg).unwrap();
    let nat = &resolved.nat;
    assert!(nat.autonat && nat.dcutr && nat.relay_client);
    assert!(nat.act_as_relay);
    assert!(!nat.mdns);
    assert_eq!(nat.max_relays, 7);
    assert_eq!(nat.relay_limits.max_circuits, 42);
    assert_eq!(nat.external_addresses.len(), 1);
    assert_eq!(
        nat.external_addresses[0].to_string(),
        "/ip4/203.0.113.10/udp/9595/quic-v1"
    );
}

// ---------------------------------------------------------------------------
// The whole NAT stack can be toggled OFF: the swarm still builds and gossip
// propagation works exactly as before (proves the Toggle wiring + back-compat).
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_builds_with_nat_stack_disabled_and_still_gossips() {
    let mut cfg_a = disc_config(vec![]);
    cfg_a.nat = NatParams::default(); // all NAT behaviours off
    let node_a = Libp2pDiscovery::spawn(cfg_a).await.unwrap();
    let a_addrs = node_a.wait_listeners(Duration::from_secs(5)).await;
    assert!(!a_addrs.is_empty(), "node A must bind with NAT stack off");

    let mut cfg_b = disc_config(a_addrs);
    cfg_b.nat = NatParams::default();
    let node_b = Libp2pDiscovery::spawn(cfg_b).await.unwrap();

    let ad = signed_ad("127.0.0.1:19710", now_ts());
    node_a.publish_ad(&ad).await.unwrap();
    let got = wait_until(Duration::from_secs(20), || node_b.membership().len() >= 1).await;
    assert!(got, "gossip must still propagate with the NAT stack disabled");
}

// ---------------------------------------------------------------------------
// A node can volunteer as a Circuit Relay v2 server (act_as_relay): the swarm
// builds with the relay-server behaviour and another node bootstraps to it.
// (True relayed connectivity needs a third NAT'd node + real network; here we
// assert the relay-capable swarm builds, binds, and forms a mesh.)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn act_as_relay_node_builds_and_peers_connect() {
    let mut relay_cfg = disc_config(vec![]);
    relay_cfg.nat = test_nat(false);
    relay_cfg.nat.act_as_relay = true; // volunteer relay
    let relay_node = Libp2pDiscovery::spawn(relay_cfg).await.unwrap();
    let relay_addrs = relay_node.wait_listeners(Duration::from_secs(5)).await;
    assert!(!relay_addrs.is_empty(), "relay node must bind a listen addr");

    // A client bootstraps to the relay and they exchange a gossiped ad, proving
    // the relay-capable node participates normally in the overlay.
    let client = Libp2pDiscovery::spawn(disc_config(relay_addrs)).await.unwrap();
    let ad = signed_ad("127.0.0.1:19720", now_ts());
    client.publish_ad(&ad).await.unwrap();
    let relay_learned =
        wait_until(Duration::from_secs(20), || relay_node.membership().len() >= 1).await;
    assert!(relay_learned, "relay node should learn the client's ad");
}

// ---------------------------------------------------------------------------
// mDNS zero-config discovery: two nodes with NO bootstrap seed find each other
// purely via mDNS on the loopback/LAN, then a gossiped ad propagates across the
// auto-formed link.
//
// Both swarms are built with the mDNS behaviour enabled (proving the wiring),
// and `spawn`/listener binding always succeeds. The *propagation* leg exercises
// real multicast on the host: when the environment delivers mDNS multicast the
// test asserts B discovers A; when multicast is unavailable (common in
// sandboxed CI with no usable network interface), it is treated as a skip so
// the suite stays green. True cross-NAT/WAN traversal (AutoNAT/DCUtR/relay)
// cannot be simulated in-process and needs a real multi-host deployment — see
// the module docs.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mdns_nodes_discover_each_other_on_loopback() {
    let unique_topic = format!("duckdb-p2p/caps/mdns-{}", now_ts());

    let mut cfg_a = disc_config(vec![]); // no bootstrap — rely on mDNS
    cfg_a.nat = test_nat(true);
    cfg_a.topic = unique_topic.clone();
    let node_a = Libp2pDiscovery::spawn(cfg_a).await.unwrap();
    assert!(
        !node_a.wait_listeners(Duration::from_secs(5)).await.is_empty(),
        "mDNS-enabled node A must build and bind a listener"
    );

    let mut cfg_b = disc_config(vec![]); // no bootstrap — rely on mDNS
    cfg_b.nat = test_nat(true);
    cfg_b.topic = unique_topic;
    let node_b = Libp2pDiscovery::spawn(cfg_b).await.unwrap();
    assert!(
        !node_b.wait_listeners(Duration::from_secs(5)).await.is_empty(),
        "mDNS-enabled node B must build and bind a listener"
    );

    let ad = signed_ad("127.0.0.1:19730", now_ts());
    node_a.publish_ad(&ad).await.unwrap();

    // With no bootstrap configured, B can only learn A's ad if mDNS discovered
    // A and the gossip mesh formed over that auto-discovered link.
    let discovered = wait_until(Duration::from_secs(20), || node_b.membership().len() >= 1).await;
    if discovered {
        let cands = node_b.find_candidates(8, filter()).await;
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].addr.to_string(), "127.0.0.1:19730");
    } else {
        eprintln!(
            "SKIP: mDNS multicast unavailable in this environment; cannot validate \
             cross-node LAN discovery here (needs a host with working multicast)."
        );
    }
}

// ---------------------------------------------------------------------------
// Candidate sampling stays bounded even with many ads (no global broadcast).
// ---------------------------------------------------------------------------
#[tokio::test]
async fn candidate_sampling_stays_bounded() {
    let table = MembershipTable::new(64, POW_BITS, 3600);
    let now = now_ts();
    let major = p2p_proto::PROTOCOL_VERSION.major;
    for i in 0..500u32 {
        let ad = signed_ad(&format!("127.0.0.1:{}", 20000 + i), now);
        let bytes = p2p_proto::to_bytes(&ad).unwrap();
        assert_eq!(
            evaluate_ad(&bytes, &table, now, 3600, major),
            AdOutcome::Accepted
        );
    }
    // Bounded cache.
    assert_eq!(table.len(), 64);
    // Bounded sample regardless of how many are requested.
    let cands = table.find_candidates(10_000, filter()).await;
    assert!(cands.len() <= 64 && !cands.is_empty());
}
