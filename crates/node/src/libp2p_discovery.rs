//! Real cross-swarm discovery propagation over **libp2p** (architecture §8).
//!
//! This is the production [`Discovery`] implementation that disseminates signed
//! [`CapabilityAd`]s across the swarm, replacing the deferred wire layer behind
//! the existing trait. It combines:
//!
//!  * **Kademlia DHT** (`libp2p::kad`) for scalable, sub-linear peer lookup and
//!    routing-table maintenance, bootstrapped from a configurable seed list.
//!  * **gossipsub** (`libp2p::gossipsub`) for propagating capability ads on a
//!    versioned topic. Every received ad is verified — signature + node-id
//!    binding + PoW (via [`MembershipTable::ingest`]) plus schema/protocol-major
//!    compatibility and freshness — before it is admitted. Malformed, expired,
//!    or incompatible-version ads are rejected.
//!
//! The verified ads land in a **bounded, LRU-evicted** [`MembershipTable`] — the
//! same scalable local view used by the in-memory impl — so candidate sampling
//! stays bounded and fan-out sub-linear regardless of swarm size. Bootstrap
//! peers are used only to *enter* the swarm; they hold no job state and are never
//! in the data path.
//!
//! Everything is configuration-driven (`[discovery]`): listen addrs, bootstrap
//! seeds, DHT replication/parallelism, gossip topic/heartbeat/fanout, ad TTL,
//! and the bounded membership cache size. Nothing is hard-coded.
//!
//! The discovery overlay runs its own libp2p identity/transport (TCP + Noise +
//! Yamux); it is **separate** from the QUIC data plane. The authenticated node
//! identity that matters for trust/selection travels *inside* the signed ad
//! (`node_id` + `pubkey` + `sig`), so the overlay PeerId is independent.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::SwarmEvent;
use libp2p::{autonat, dcutr, gossipsub, identify, kad, mdns, noise, relay, tcp, yamux};
use libp2p::{Multiaddr, PeerId};
use p2p_config::GridConfig;
use p2p_proto::CapabilityAd;
use p2p_trust::now_ts;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::discovery::{Candidate, CandidateFilter, Discovery};
use crate::membership::MembershipTable;

/// Errors building or driving the libp2p discovery overlay.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("invalid multiaddr {0:?}: {1}")]
    BadMultiaddr(String, String),
    #[error("libp2p build error: {0}")]
    Build(String),
    #[error("transport/listen error: {0}")]
    Transport(String),
    #[error("discovery task has stopped")]
    TaskStopped,
}

/// Resolved, validated parameters for the libp2p discovery overlay. Built from
/// [`GridConfig`] so nothing is hard-coded.
#[derive(Debug, Clone)]
pub struct Libp2pDiscoveryConfig {
    pub listen_addrs: Vec<Multiaddr>,
    pub bootstrap: Vec<Multiaddr>,
    pub topic: String,
    pub heartbeat: Duration,
    pub mesh_n: usize,
    pub capability_ttl_secs: u64,
    pub required_pow_bits: u32,
    pub membership_capacity: usize,
    pub replication_factor: usize,
    pub query_parallelism: usize,
    pub protocol_major: u16,
    /// Global NAT-traversal stack parameters (AutoNAT, DCUtR, Circuit Relay v2 +
    /// AutoRelay, mDNS). Built from `[discovery.nat]`.
    pub nat: NatParams,
    /// Enable gossipsub peer scoring (eclipse/gossip hardening, ARCHITECTURE
    /// "Abuse resistance"). Built from `[antiabuse.gossip].peer_scoring`.
    pub gossip_peer_scoring: bool,
    /// Prefer a diverse bootstrap/relay set (shuffle entry points) to resist
    /// eclipse. Built from `[antiabuse.gossip].diverse_bootstrap`.
    pub diverse_bootstrap: bool,
}

/// Resolved NAT-traversal parameters for the overlay (parsed from
/// [`p2p_config::NatConfig`] so nothing is hard-coded).
#[derive(Debug, Clone)]
pub struct NatParams {
    pub autonat: bool,
    pub dcutr: bool,
    pub relay_client: bool,
    pub act_as_relay: bool,
    pub mdns: bool,
    pub mdns_query_interval: Duration,
    pub external_addresses: Vec<Multiaddr>,
    pub relays: Vec<Multiaddr>,
    pub max_relays: usize,
    pub relay_limits: RelayLimits,
}

impl Default for NatParams {
    fn default() -> Self {
        Self {
            autonat: false,
            dcutr: false,
            relay_client: false,
            act_as_relay: false,
            mdns: false,
            mdns_query_interval: Duration::from_secs(300),
            external_addresses: Vec::new(),
            relays: Vec::new(),
            max_relays: 3,
            relay_limits: RelayLimits::default(),
        }
    }
}

/// Resolved volunteer-relay server limits (mirrors [`relay::Config`] caps).
#[derive(Debug, Clone)]
pub struct RelayLimits {
    pub max_reservations: usize,
    pub max_reservations_per_peer: usize,
    pub reservation_duration: Duration,
    pub max_circuits: usize,
    pub max_circuits_per_peer: usize,
    pub max_circuit_duration: Duration,
    pub max_circuit_bytes: u64,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_reservations: 128,
            max_reservations_per_peer: 4,
            reservation_duration: Duration::from_secs(60 * 60),
            max_circuits: 16,
            max_circuits_per_peer: 4,
            max_circuit_duration: Duration::from_secs(2 * 60),
            max_circuit_bytes: 1 << 17,
        }
    }
}

impl RelayLimits {
    /// Build a [`relay::Config`] from these limits (keeping the library's
    /// default rate limiters).
    fn to_relay_config(&self) -> relay::Config {
        relay::Config {
            max_reservations: self.max_reservations,
            max_reservations_per_peer: self.max_reservations_per_peer,
            reservation_duration: self.reservation_duration,
            max_circuits: self.max_circuits,
            max_circuits_per_peer: self.max_circuits_per_peer,
            max_circuit_duration: self.max_circuit_duration,
            max_circuit_bytes: self.max_circuit_bytes,
            ..Default::default()
        }
    }
}

fn parse_multiaddrs(raw: &[String]) -> Result<Vec<Multiaddr>, DiscoveryError> {
    raw.iter()
        .map(|s| {
            s.parse::<Multiaddr>()
                .map_err(|e| DiscoveryError::BadMultiaddr(s.clone(), e.to_string()))
        })
        .collect()
}

impl Libp2pDiscoveryConfig {
    /// Derive overlay parameters from the full grid config.
    pub fn from_grid(cfg: &GridConfig) -> Result<Self, DiscoveryError> {
        let protocol_major = cfg
            .protocol
            .version
            .parse::<p2p_proto::Version>()
            .map(|v| v.major)
            .unwrap_or(p2p_proto::PROTOCOL_VERSION.major);
        Ok(Self {
            listen_addrs: parse_multiaddrs(&cfg.discovery.listen_addrs)?,
            bootstrap: parse_multiaddrs(&cfg.discovery.bootstrap)?,
            topic: cfg.discovery.gossip.topic.clone(),
            heartbeat: Duration::from_millis(cfg.discovery.gossip.heartbeat_ms.max(1)),
            mesh_n: cfg.discovery.gossip.fanout.max(1),
            capability_ttl_secs: cfg.discovery.gossip.capability_ttl_secs,
            required_pow_bits: cfg.sybil.pow_difficulty_bits,
            membership_capacity: cfg.limits.peer_cache_capacity.max(1),
            replication_factor: cfg.discovery.kademlia.replication_factor.max(1),
            query_parallelism: cfg.discovery.kademlia.query_parallelism.max(1),
            protocol_major,
            nat: NatParams::from_config(&cfg.discovery.nat)?,
            gossip_peer_scoring: cfg.antiabuse.enabled && cfg.antiabuse.gossip.peer_scoring,
            diverse_bootstrap: cfg.antiabuse.gossip.diverse_bootstrap,
        })
    }
}

impl NatParams {
    /// Resolve NAT-traversal parameters from the `[discovery.nat]` config.
    pub fn from_config(cfg: &p2p_config::NatConfig) -> Result<Self, DiscoveryError> {
        Ok(Self {
            autonat: cfg.autonat,
            dcutr: cfg.dcutr,
            relay_client: cfg.relay_client,
            act_as_relay: cfg.act_as_relay,
            mdns: cfg.mdns,
            mdns_query_interval: Duration::from_secs(cfg.mdns_query_interval_secs.max(1)),
            external_addresses: parse_multiaddrs(&cfg.external_addresses)?,
            relays: parse_multiaddrs(&cfg.relays)?,
            max_relays: cfg.max_relays.max(1),
            relay_limits: RelayLimits {
                max_reservations: cfg.relay_limits.max_reservations,
                max_reservations_per_peer: cfg.relay_limits.max_reservations_per_peer,
                reservation_duration: Duration::from_secs(cfg.relay_limits.reservation_duration_secs),
                max_circuits: cfg.relay_limits.max_circuits,
                max_circuits_per_peer: cfg.relay_limits.max_circuits_per_peer,
                max_circuit_duration: Duration::from_secs(cfg.relay_limits.max_circuit_duration_secs),
                max_circuit_bytes: cfg.relay_limits.max_circuit_bytes,
            },
        })
    }
}

/// Outcome of validating an incoming gossiped ad. Exposed for metrics/testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdOutcome {
    /// Verified and stored in the bounded membership view.
    Accepted,
    /// Bytes did not decode into a `CapabilityAd`.
    Malformed,
    /// Schema/protocol major mismatch with this node.
    IncompatibleVersion,
    /// `ts` outside the freshness window (too old, or implausibly far future).
    Expired,
    /// Signature, node-id binding, or PoW failed verification.
    Rejected,
}

/// Decide whether a raw gossip payload is an acceptable capability ad. Performs
/// version-compatibility + freshness checks, then signature/PoW verification by
/// ingesting into the bounded membership table. Pure-ish (mutates the table only
/// on `Accepted`) so the policy is unit-testable.
pub fn evaluate_ad(
    data: &[u8],
    membership: &MembershipTable,
    now: u64,
    ttl_secs: u64,
    expected_major: u16,
) -> AdOutcome {
    let ad: CapabilityAd = match p2p_proto::from_bytes(data) {
        Ok(a) => a,
        Err(_) => return AdOutcome::Malformed,
    };
    if ad.schema_version != p2p_proto::SCHEMA_VERSION
        || ad.protocol_version.major != expected_major
    {
        return AdOutcome::IncompatibleVersion;
    }
    // Reject stale ads and implausibly far-future timestamps (small clock skew
    // is tolerated).
    const FUTURE_SKEW_SECS: u64 = 60;
    if now.saturating_sub(ad.ts) > ttl_secs || ad.ts > now.saturating_add(FUTURE_SKEW_SECS) {
        return AdOutcome::Expired;
    }
    // Signature + node-id binding + PoW are enforced by the membership cache.
    if membership.ingest(ad) {
        AdOutcome::Accepted
    } else {
        AdOutcome::Rejected
    }
}

/// Combined network behaviour for the discovery overlay.
///
/// Beyond Kademlia + gossipsub + identify, this carries the global NAT-traversal
/// stack. The NAT behaviours are [`Toggle`]d so each can be enabled/disabled from
/// `[discovery.nat]` config without changing the transport or swarm type.
#[derive(libp2p::swarm::NetworkBehaviour)]
struct DiscoveryBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    identify: identify::Behaviour,
    /// AutoNAT: detect public reachability + learn external address.
    autonat: Toggle<autonat::Behaviour>,
    /// DCUtR: relay-assisted direct-connection upgrade (hole punching).
    dcutr: Toggle<dcutr::Behaviour>,
    /// Circuit Relay v2 client (reserve circuits on volunteer relays).
    relay_client: Toggle<relay::client::Behaviour>,
    /// Circuit Relay v2 server (volunteer to relay for others).
    relay_server: Toggle<relay::Behaviour>,
    /// mDNS zero-config LAN peer discovery.
    mdns: Toggle<mdns::tokio::Behaviour>,
}

enum Command {
    /// Set/replace the local capability ad to (re)publish each heartbeat.
    SetAd(Vec<u8>),
}

/// A running libp2p discovery overlay implementing [`Discovery`].
pub struct Libp2pDiscovery {
    membership: Arc<MembershipTable>,
    cmd_tx: mpsc::Sender<Command>,
    local_peer_id: PeerId,
    listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Libp2pDiscovery {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Libp2pDiscovery {
    /// Convenience: build the overlay from a full [`GridConfig`].
    pub async fn from_config(cfg: &GridConfig) -> Result<Self, DiscoveryError> {
        Self::spawn(Libp2pDiscoveryConfig::from_grid(cfg)?).await
    }

    /// Build the overlay, start listening, dial/bootstrap seeds, and spawn the
    /// background swarm event loop. Returns once the swarm is constructed.
    pub async fn spawn(cfg: Libp2pDiscoveryConfig) -> Result<Self, DiscoveryError> {
        let membership = Arc::new(MembershipTable::new(
            cfg.membership_capacity,
            cfg.required_pow_bits,
            cfg.capability_ttl_secs,
        ));

        let heartbeat = cfg.heartbeat;
        let mesh_n = cfg.mesh_n;
        let replication = cfg.replication_factor;
        let parallelism = cfg.query_parallelism;
        let peer_scoring = cfg.gossip_peer_scoring;
        // NAT params: a clone is moved into the (FnOnce) behaviour constructor;
        // the original `cfg.nat` is reused afterwards for listen/external-addr
        // wiring.
        let nat = cfg.nat.clone();

        // Transport stack: TCP+Noise+Yamux *and* QUIC (UDP) — DCUtR hole punching
        // and direct dials use QUIC/UDP — plus a Circuit Relay v2 *client*
        // transport so the node can be dialed/listen via volunteer relays. The
        // relay client behaviour is handed to the behaviour constructor.
        let mut swarm = libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| DiscoveryError::Build(e.to_string()))?
            .with_quic()
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| DiscoveryError::Build(e.to_string()))?
            .with_behaviour(|key, relay_client_behaviour| {
                let peer_id = key.public().to_peer_id();

                // gossipsub: republish-friendly, small-mesh tolerant.
                let mesh_low = 1.max(mesh_n / 2);
                let mesh_high = (mesh_n * 2).max(mesh_n + 1);
                let gossip_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(heartbeat)
                    .mesh_n(mesh_n)
                    .mesh_n_low(mesh_low)
                    .mesh_n_high(mesh_high)
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .build()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                let mut gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg,
                )
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                // Eclipse/gossip hardening: enable gossipsub peer scoring so
                // misbehaving mesh peers are penalized and pruned (ARCHITECTURE
                // "Abuse resistance"). Default-lenient library params; off unless
                // [antiabuse.gossip].peer_scoring is set.
                if peer_scoring {
                    let params = gossipsub::PeerScoreParams::default();
                    let thresholds = gossipsub::PeerScoreThresholds::default();
                    gossipsub
                        .with_peer_score(params, thresholds)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                }

                // Kademlia in server mode with configured replication/parallelism.
                let mut kad_cfg = kad::Config::default();
                if let Some(r) = NonZeroUsize::new(replication) {
                    kad_cfg.set_replication_factor(r);
                }
                if let Some(p) = NonZeroUsize::new(parallelism) {
                    kad_cfg.set_parallelism(p);
                }
                let store = kad::store::MemoryStore::new(peer_id);
                let mut kademlia = kad::Behaviour::with_config(peer_id, store, kad_cfg);
                kademlia.set_mode(Some(kad::Mode::Server));

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/duckdb-p2p-disc/1.0.0".into(),
                    key.public(),
                ));

                // --- Global NAT-traversal stack (each toggled by config) ---
                let autonat = Toggle::from(nat.autonat.then(|| {
                    autonat::Behaviour::new(peer_id, autonat::Config::default())
                }));
                let dcutr = Toggle::from(nat.dcutr.then(|| dcutr::Behaviour::new(peer_id)));
                let relay_client =
                    Toggle::from(nat.relay_client.then_some(relay_client_behaviour));
                let relay_server = Toggle::from(nat.act_as_relay.then(|| {
                    relay::Behaviour::new(peer_id, nat.relay_limits.to_relay_config())
                }));
                let mdns = Toggle::from(if nat.mdns {
                    let mdns_cfg = mdns::Config {
                        query_interval: nat.mdns_query_interval,
                        ..Default::default()
                    };
                    match mdns::tokio::Behaviour::new(mdns_cfg, peer_id) {
                        Ok(b) => Some(b),
                        Err(e) => {
                            warn!("mDNS disabled: failed to start ({e})");
                            None
                        }
                    }
                } else {
                    None
                });

                Ok(DiscoveryBehaviour {
                    gossipsub,
                    kademlia,
                    identify,
                    autonat,
                    dcutr,
                    relay_client,
                    relay_server,
                    mdns,
                })
            })
            .map_err(|e| DiscoveryError::Build(e.to_string()))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(300)))
            .build();

        let local_peer_id = *swarm.local_peer_id();
        let topic = gossipsub::IdentTopic::new(cfg.topic.clone());
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&topic)
            .map_err(|e| DiscoveryError::Build(e.to_string()))?;

        // Listen: explicit addrs, or an ephemeral loopback TCP port for tests.
        if cfg.listen_addrs.is_empty() {
            let addr: Multiaddr = "/ip4/127.0.0.1/tcp/0"
                .parse()
                .expect("static loopback multiaddr is valid");
            swarm
                .listen_on(addr)
                .map_err(|e| DiscoveryError::Transport(e.to_string()))?;
        } else {
            for addr in &cfg.listen_addrs {
                swarm
                    .listen_on(addr.clone())
                    .map_err(|e| DiscoveryError::Transport(e.to_string()))?;
            }
        }

        // Seed the routing table and dial bootstrap peers to enter the swarm.
        // Eclipse hardening: when `diverse_bootstrap` is on, randomize the dial
        // order so a node does not deterministically favor one entry point that
        // could eclipse it (ARCHITECTURE "Abuse resistance").
        let mut bootstrap = cfg.bootstrap.clone();
        if cfg.diverse_bootstrap {
            use rand::seq::SliceRandom;
            bootstrap.shuffle(&mut rand::thread_rng());
        }
        for addr in &bootstrap {
            if let Some(peer) = peer_id_from_multiaddr(addr) {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer, addr.clone());
            }
            if let Err(e) = swarm.dial(addr.clone()) {
                debug!("dial bootstrap {addr} failed: {e}");
            }
        }
        if !bootstrap.is_empty() {
            let _ = swarm.behaviour_mut().kademlia.bootstrap();
        }

        // Advertise any operator-supplied external addresses (augments anything
        // AutoNAT later discovers). These are the "no fixed IP/URL" override.
        for addr in &cfg.nat.external_addresses {
            swarm.add_external_address(addr.clone());
        }

        // Reserve a relay circuit on each explicitly-configured relay so an
        // unreachable node is dialable immediately (before AutoRelay kicks in).
        // Listening on `<relay>/p2p-circuit` is how the relay client requests a
        // reservation. AutoRelay (in the driver) tops this up from relays
        // discovered on the network, bounded by `max_relays`.
        let mut reserved_relays: HashSet<PeerId> = HashSet::new();
        if cfg.nat.relay_client {
            for relay_addr in cfg.nat.relays.iter().take(cfg.nat.max_relays) {
                if let Some(peer) = peer_id_from_multiaddr(relay_addr) {
                    swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer, relay_addr.clone());
                    reserved_relays.insert(peer);
                }
                let circuit = relay_addr.clone().with(Protocol::P2pCircuit);
                if let Err(e) = swarm.listen_on(circuit) {
                    debug!("relay reservation on {relay_addr} failed: {e}");
                }
            }
        }

        let listen_addrs = Arc::new(Mutex::new(Vec::new()));
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);

        let driver = SwarmDriver {
            swarm,
            topic,
            membership: membership.clone(),
            listen_addrs: listen_addrs.clone(),
            current_ad: None,
            ttl_secs: cfg.capability_ttl_secs,
            protocol_major: cfg.protocol_major,
            heartbeat,
            relay_client_enabled: cfg.nat.relay_client,
            max_relays: cfg.nat.max_relays,
            reserved_relays,
        };
        let task = tokio::spawn(driver.run(cmd_rx));

        Ok(Self {
            membership,
            cmd_tx,
            local_peer_id,
            listen_addrs,
            task,
        })
    }

    /// The overlay PeerId (used to form `/p2p/<id>` bootstrap multiaddrs).
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// The bounded membership view backing candidate sampling.
    pub fn membership(&self) -> &Arc<MembershipTable> {
        &self.membership
    }

    /// Publish (and keep republishing each heartbeat) this node's signed
    /// capability ad to the gossip topic.
    pub async fn publish_ad(&self, ad: &CapabilityAd) -> Result<(), DiscoveryError> {
        let bytes = p2p_proto::to_bytes(ad).map_err(|e| DiscoveryError::Build(e.to_string()))?;
        self.cmd_tx
            .send(Command::SetAd(bytes))
            .await
            .map_err(|_| DiscoveryError::TaskStopped)
    }

    /// Current bound listen multiaddrs, each with `/p2p/<peer_id>` appended so
    /// they can be handed to another node as a bootstrap seed.
    pub fn listeners(&self) -> Vec<Multiaddr> {
        let addrs = self.listen_addrs.lock().unwrap().clone();
        addrs
            .into_iter()
            .map(|a| a.with(libp2p::multiaddr::Protocol::P2p(self.local_peer_id)))
            .collect()
    }

    /// Wait until at least one listen address is bound (or the timeout elapses).
    /// Returns the dialable bootstrap multiaddrs.
    pub async fn wait_listeners(&self, timeout: Duration) -> Vec<Multiaddr> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let addrs = self.listeners();
            if !addrs.is_empty() {
                return addrs;
            }
            if tokio::time::Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[async_trait]
impl Discovery for Libp2pDiscovery {
    async fn find_candidates(&self, want: usize, filter: CandidateFilter) -> Vec<Candidate> {
        self.membership.find_candidates(want, filter).await
    }
}

/// Extract the `/p2p/<peer_id>` component of a multiaddr, if present.
fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| match p {
        libp2p::multiaddr::Protocol::P2p(peer) => Some(peer),
        _ => None,
    })
}

struct SwarmDriver {
    swarm: libp2p::Swarm<DiscoveryBehaviour>,
    topic: gossipsub::IdentTopic,
    membership: Arc<MembershipTable>,
    listen_addrs: Arc<Mutex<Vec<Multiaddr>>>,
    current_ad: Option<Vec<u8>>,
    ttl_secs: u64,
    protocol_major: u16,
    heartbeat: Duration,
    /// AutoRelay: whether to auto-reserve circuits on relays learned from the
    /// network, and the cap on simultaneous relay reservations.
    relay_client_enabled: bool,
    max_relays: usize,
    /// Relay peers we already hold (or requested) a reservation with.
    reserved_relays: HashSet<PeerId>,
}

impl SwarmDriver {
    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        let mut republish = tokio::time::interval(self.heartbeat);
        republish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(Command::SetAd(bytes)) => {
                        self.current_ad = Some(bytes);
                        self.try_publish();
                    }
                    None => break, // handle dropped
                },
                _ = republish.tick() => {
                    self.try_publish();
                }
                event = self.swarm.select_next_some() => {
                    self.on_event(event);
                }
            }
        }
    }

    fn try_publish(&mut self) {
        if let Some(bytes) = &self.current_ad {
            match self
                .swarm
                .behaviour_mut()
                .gossipsub
                .publish(self.topic.clone(), bytes.clone())
            {
                Ok(_) => {}
                Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {
                    // No subscribed mesh peers yet; the next heartbeat will retry.
                    debug!("gossip publish deferred: no subscribed peers yet");
                }
                Err(e) => debug!("gossip publish error: {e}"),
            }
        }
    }

    fn on_event(&mut self, event: SwarmEvent<DiscoveryBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                self.listen_addrs.lock().unwrap().push(address);
            }
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::Gossipsub(
                gossipsub::Event::Message { message, .. },
            )) => {
                let outcome = evaluate_ad(
                    &message.data,
                    &self.membership,
                    now_ts(),
                    self.ttl_secs,
                    self.protocol_major,
                );
                if outcome != AdOutcome::Accepted {
                    debug!("rejected gossiped ad: {outcome:?}");
                }
            }
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::Identify(
                identify::Event::Received { peer_id, info, .. },
            )) => {
                // Learn the peer's addresses for Kademlia routing and add it as a
                // gossip peer so small meshes form promptly.
                for addr in &info.listen_addrs {
                    self.swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, addr.clone());
                }
                self.swarm
                    .behaviour_mut()
                    .gossipsub
                    .add_explicit_peer(&peer_id);
                // AutoRelay: if this peer is a Circuit Relay v2 server and we are
                // still under our reservation cap, reserve a circuit through it
                // so unreachable peers can dial us via a VOLUNTEER relay (no
                // central server). Relays are auto-selected from the network.
                self.maybe_reserve_relay(peer_id, &info);
            }
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::Autonat(
                autonat::Event::StatusChanged { new, .. },
            )) => {
                // AutoNAT decided we are publicly reachable at `addr`: advertise
                // it so peers can dial us directly (learned, not hard-coded).
                if let autonat::NatStatus::Public(addr) = new {
                    debug!("autonat: publicly reachable at {addr}");
                    self.swarm.add_external_address(addr);
                } else {
                    debug!("autonat status: {new:?}");
                }
            }
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::RelayClient(
                relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
            )) => {
                debug!("relay reservation accepted by {relay_peer_id}");
                self.reserved_relays.insert(relay_peer_id);
            }
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::Dcutr(dcutr::Event {
                remote_peer_id,
                result,
            })) => match result {
                Ok(_) => debug!("dcutr: direct connection upgraded to {remote_peer_id}"),
                Err(e) => debug!("dcutr: hole punch to {remote_peer_id} failed: {e}"),
            },
            SwarmEvent::Behaviour(DiscoveryBehaviourEvent::Mdns(mdns::Event::Discovered(
                peers,
            ))) => {
                // Zero-config LAN discovery: route + gossip-peer each found node.
                for (peer_id, addr) in peers {
                    self.swarm
                        .behaviour_mut()
                        .kademlia
                        .add_address(&peer_id, addr.clone());
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .add_explicit_peer(&peer_id);
                    if let Err(e) = self.swarm.dial(addr.clone()) {
                        debug!("mdns dial {addr} failed: {e}");
                    }
                }
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                self.swarm
                    .behaviour_mut()
                    .gossipsub
                    .add_explicit_peer(&peer_id);
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                warn!("outgoing connection error to {peer_id:?}: {error}");
            }
            _ => {}
        }
    }

    /// AutoRelay: reserve a relayed circuit through `peer` if it advertises the
    /// Circuit Relay v2 hop protocol and we are still under `max_relays`. This is
    /// how a node behind a symmetric NAT (where hole punching can't succeed)
    /// stays dialable — through volunteer peers discovered on the network, never
    /// a central server.
    fn maybe_reserve_relay(&mut self, peer: PeerId, info: &identify::Info) {
        if !self.relay_client_enabled
            || self.reserved_relays.len() >= self.max_relays
            || self.reserved_relays.contains(&peer)
        {
            return;
        }
        let is_relay = info
            .protocols
            .iter()
            .any(|p| *p == relay::HOP_PROTOCOL_NAME);
        if !is_relay {
            return;
        }
        // Pick a concrete (non-relayed) listen address to reach the relay on.
        let Some(base) = info
            .listen_addrs
            .iter()
            .find(|a| !a.iter().any(|p| matches!(p, Protocol::P2pCircuit)))
            .cloned()
        else {
            return;
        };
        let circuit = base
            .with(Protocol::P2p(peer))
            .with(Protocol::P2pCircuit);
        match self.swarm.listen_on(circuit) {
            Ok(_) => {
                debug!("autorelay: reserving circuit via relay {peer}");
                self.reserved_relays.insert(peer);
            }
            Err(e) => debug!("autorelay: reservation via {peer} failed: {e}"),
        }
    }
}
