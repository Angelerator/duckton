//! `p2p-config` — the single source of truth for every operational value.
//!
//! Design principles (mandated cross-cutting requirements):
//!
//! * **No hard-coding.** Ports, addresses, seeds, timeouts, retry/backoff,
//!   replica count `k`, quorum, trust thresholds, attestation level, budgets,
//!   canary rate, reputation decay/weights, Sybil PoW difficulty, key TTLs,
//!   cache sizes, concurrency limits — all live here as typed, documented
//!   fields with sensible defaults.
//!
//! * **Layered precedence (lowest → highest):**
//!     1. Built-in defaults ([`Default`] impls below).
//!     2. Config file (TOML) — [`GridConfig::from_toml_file`].
//!     3. Environment variables (`P2P_*`) — [`GridConfig::apply_env`].
//!     4. Per-call SQL parameters (args to `p2p_query` / `p2p_share` /
//!        `p2p_join`) — the `*Overrides` structs.
//!
//!   [`GridConfig::load`] runs layers 1–3; the node/extension layer then
//!   applies per-call overrides on top.
//!
//! * **Validation.** [`GridConfig::validate`] enforces cross-field invariants
//!   (e.g. `quorum <= replicas`, thresholds in `[0,1]`).
//!
//! Durations are expressed as explicit `_ms` / `_secs` integer fields to keep
//! the TOML unambiguous and the values deterministic.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

mod antiabuse;
mod blocklist;
mod economics;
mod overrides;
mod store;
pub use antiabuse::{
    AntiAbuseConfig, BlocklistPolicyConfig, CostGateConfig, FaultAttributionConfig,
    FreeRateLimitConfig, GossipHardeningConfig, NondeterminismConfig, RequesterTrustConfig,
};
pub use blocklist::{BlockEntry, BlockKind, BlocklistStore};
pub use economics::{
    ContractsConfig, EconomicsConfig, FeesEconomics, NetworkSettings, PaymentMode, PaymentPref,
    PricingEconomics, QualityEconomics, RankingEconomics, RecordsEconomics, ReputationEconomics,
    SelectionEconomics, SettlementRail, SlashingEconomics, StakeEconomics, TonNetwork, WalletConfig,
};
pub use overrides::{JoinOverrides, QueryOverrides, ShareOverrides};
pub use store::{flatten_settings, status_rows, ConfigStore, SettingRow, StoreError};

/// Errors from loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {0}: {1}")]
    Io(String, std::io::Error),
    #[error("failed to parse TOML config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid environment variable {0}: {1}")]
    Env(String, String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Top-level configuration for a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridConfig {
    pub protocol: ProtocolConfig,
    pub network: NetworkConfig,
    /// Transport performance tuning (UDP offload, flow-control sizing,
    /// congestion control, parallel result transfer, wire compression, 0-RTT).
    pub transport: TransportConfig,
    pub identity: IdentityConfig,
    pub discovery: DiscoveryConfig,
    pub scheduler: SchedulerConfig,
    /// Host (worker) execution deadline + progress-streaming interval
    /// (resilience layer, architecture §11).
    pub worker: WorkerConfig,
    /// Liveness / failure detection (phi-accrual + SWIM, architecture §8).
    pub liveness: LivenessConfig,
    pub budget: BudgetConfig,
    pub trust: TrustConfig,
    pub sybil: SybilConfig,
    pub storage: StorageConfig,
    /// Local-first vs grid-dispatch query planner (free in-process local
    /// execution, data-size estimation, headroom-based routing, adaptive
    /// fail-over). See [`PlannerConfig`].
    pub planner: PlannerConfig,
    /// Blockchain economic / settlement layer (off by default; see
    /// `docs/BLOCKCHAIN_ECONOMICS.md`). When `economics.enabled = false` the node
    /// behaves exactly as today: free, no chain, but still scored.
    pub economics: EconomicsConfig,
    /// Anti-abuse / robustness layer (fault attribution, requester-trust
    /// weighting, pre-flight cost gating, deny-lists, non-determinism handling,
    /// free-mode rate limiting, gossip hardening). Defaults preserve today's
    /// behavior where a change would be observable. See `docs/ARCHITECTURE.md`
    /// "Abuse resistance".
    pub antiabuse: AntiAbuseConfig,
    pub limits: LimitsConfig,
    /// OS-level execution sandbox wrapped AROUND job execution (architecture
    /// §9.4): rlimit/cgroups/seccomp resource caps + network egress restricted
    /// to the configured storage endpoints. Off by default (`enabled = false`)
    /// so existing behavior is unchanged — a no-op sandbox. This is the
    /// complement DuckDB cannot provide (it cannot scope network egress).
    pub sandbox: SandboxConfig,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            protocol: ProtocolConfig::default(),
            network: NetworkConfig::default(),
            transport: TransportConfig::default(),
            identity: IdentityConfig::default(),
            discovery: DiscoveryConfig::default(),
            scheduler: SchedulerConfig::default(),
            worker: WorkerConfig::default(),
            liveness: LivenessConfig::default(),
            budget: BudgetConfig::default(),
            trust: TrustConfig::default(),
            sybil: SybilConfig::default(),
            storage: StorageConfig::default(),
            planner: PlannerConfig::default(),
            economics: EconomicsConfig::default(),
            antiabuse: AntiAbuseConfig::default(),
            limits: LimitsConfig::default(),
            sandbox: SandboxConfig::default(),
        }
    }
}

/// Protocol versioning & compatibility policy (architecture §5.1). Centralized
/// so ALPN + handshake + negotiation all derive from configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProtocolConfig {
    /// The protocol version this node advertises (semver "major.minor.patch").
    pub version: String,
    /// The minimum peer protocol version this node will accept.
    pub min_supported_version: String,
    /// Require peers to report the same DuckDB engine version for quorum
    /// participation (result-determinism, architecture §15).
    pub require_matching_engine_version: bool,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            version: p2p_proto::PROTOCOL_VERSION.to_string(),
            min_supported_version: p2p_proto::MIN_SUPPORTED_VERSION.to_string(),
            require_matching_engine_version: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// QUIC transport / endpoint settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Address to bind the QUIC endpoint to (host:port). `0` port = ephemeral.
    pub bind_addr: String,
    /// Optional externally-reachable address advertised to peers.
    pub advertised_addr: Option<String>,
    /// How long an idle connection is kept before closing (ms).
    pub idle_timeout_ms: u64,
    /// Connection establishment timeout (ms).
    pub connect_timeout_ms: u64,
    /// Keepalive ping interval (ms); should be < idle_timeout_ms.
    pub keepalive_ms: u64,
    /// Max concurrent bidirectional streams per connection (backpressure).
    pub max_concurrent_bidi_streams: u32,
    /// Per-stream receive window (bytes) — flow control / backpressure.
    pub stream_receive_window: u64,
    /// Whole-connection receive window (bytes).
    pub receive_window: u64,
    /// Size of each result chunk streamed back (bytes). Bulk results are split
    /// into chunks so QUIC stream flow-control applies backpressure.
    pub result_chunk_bytes: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:0".to_string(),
            advertised_addr: None,
            idle_timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
            keepalive_ms: 10_000,
            max_concurrent_bidi_streams: 256,
            stream_receive_window: 8 * 1024 * 1024,
            receive_window: 64 * 1024 * 1024,
            result_chunk_bytes: 256 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// Transport performance tuning (`[transport]`)
// ---------------------------------------------------------------------------

/// Transport performance tuning, grouped under `[transport]`. Every value has a
/// documented default; nothing is hard-coded in the transport/node layers. See
/// the "Transport performance tuning" section of ARCHITECTURE.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportConfig {
    /// QUIC-level knobs: UDP offload, congestion control, pacing, flow-control
    /// windows, uni-stream cap, 0-RTT. (`[transport.quic]`)
    pub quic: QuicTuningConfig,
    /// Bulk result-transfer tuning: stream parallelism + chunking.
    /// (`[transport.result]`)
    pub result: ResultTransferConfig,
    /// Optional wire compression for result data. (`[transport.compression]`)
    pub compression: CompressionConfig,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            quic: QuicTuningConfig::default(),
            result: ResultTransferConfig::default(),
            compression: CompressionConfig::default(),
        }
    }
}

/// Congestion-control algorithm selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CongestionAlgo {
    /// BBR — model-based; usually best for high bandwidth-delay-product WAN
    /// links. Available in the pinned Quinn version.
    Bbr,
    /// CUBIC — loss-based; the Quinn default and a solid general-purpose choice.
    Cubic,
    /// NewReno — conservative loss-based; mostly for comparison/testing.
    #[serde(rename = "newreno")]
    NewReno,
}

/// QUIC-level performance knobs (`[transport.quic]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuicTuningConfig {
    /// Enable UDP Generic Segmentation Offload (GSO) on transmit. The single
    /// biggest CPU/throughput lever for bulk sends. `quinn-udp` auto-disables it
    /// if the OS/NIC lacks support, so it is safe to leave on. Default: on.
    pub gso: bool,
    /// Desire UDP Generic Receive Offload (GRO) on receive. `quinn-udp` enables
    /// GRO automatically where supported and there is no per-endpoint opt-out in
    /// the pinned version, so this flag is advisory/observability only. Default: on.
    pub gro: bool,
    /// Congestion controller: "bbr" | "cubic" | "newreno".
    pub congestion: CongestionAlgo,
    /// Enable send pacing. NOTE: the pinned Quinn version always paces internally
    /// and exposes no runtime toggle, so setting this to `false` is currently
    /// advisory (documented limitation). Default: on.
    pub pacing: bool,
    /// Per-stream receive window (bytes). `None` inherits `network.stream_receive_window`.
    pub stream_receive_window_bytes: Option<u64>,
    /// Whole-connection receive window (bytes). `None` inherits `network.receive_window`.
    pub connection_receive_window_bytes: Option<u64>,
    /// Connection send window (bytes) — caps total in-flight unacked data so a
    /// fast sender does not outrun memory. Should be >= the peer's receive window
    /// to avoid being send-limited on a fat pipe.
    pub send_window_bytes: u64,
    /// Max concurrent unidirectional streams per connection. Bounds the bulk
    /// result fan-out (must be >= `transport.result.parallelism`).
    pub max_concurrent_uni_streams: u32,
    /// Enable 0-RTT / TLS session resumption for repeat peers. See the docs for
    /// the replay-safety caveat. Default: off.
    pub enable_0rtt: bool,
    /// Lifetime of issued TLS session-resumption tickets (secs).
    pub session_ticket_lifetime_secs: u64,
    /// Optional bandwidth-delay-product target. When enabled, the flow-control
    /// windows are sized to `bandwidth * rtt` so a single large result is never
    /// window-limited; this overrides the explicit window fields above.
    pub bdp: BdpConfig,
}

impl Default for QuicTuningConfig {
    fn default() -> Self {
        Self {
            gso: true,
            gro: true,
            congestion: CongestionAlgo::Cubic,
            pacing: true,
            stream_receive_window_bytes: None,
            connection_receive_window_bytes: None,
            send_window_bytes: 64 * 1024 * 1024,
            max_concurrent_uni_streams: 256,
            enable_0rtt: false,
            session_ticket_lifetime_secs: 12 * 3600,
            bdp: BdpConfig::default(),
        }
    }
}

impl QuicTuningConfig {
    /// Resolve the effective `(stream_receive_window, connection_receive_window,
    /// send_window)` in bytes, applying the precedence: BDP target (if enabled)
    /// > explicit override > `network` defaults.
    pub fn effective_windows(&self, net: &NetworkConfig) -> (u64, u64, u64) {
        if self.bdp.enabled {
            let target = self.bdp.target_bytes();
            // Connection window holds several streams' worth; send window matches
            // the target so we can keep the pipe full.
            (target, target.saturating_mul(2).max(target), target.saturating_mul(2))
        } else {
            let stream = self.stream_receive_window_bytes.unwrap_or(net.stream_receive_window);
            let conn = self.connection_receive_window_bytes.unwrap_or(net.receive_window);
            (stream, conn, self.send_window_bytes)
        }
    }
}

/// Bandwidth-delay-product target used to auto-size flow-control windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BdpConfig {
    /// When true, derive the flow-control windows from `bandwidth_mbps * rtt_ms`.
    pub enabled: bool,
    /// Target end-to-end link bandwidth in megabits/second.
    pub bandwidth_mbps: u32,
    /// Target round-trip time in milliseconds.
    pub rtt_ms: u32,
}

impl Default for BdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bandwidth_mbps: 1000,
            rtt_ms: 50,
        }
    }
}

impl BdpConfig {
    /// Bandwidth-delay product in bytes: `(mbps * 1e6 / 8) * (rtt_ms / 1000)`.
    ///
    /// Uses saturating arithmetic so a large operator-supplied
    /// `bandwidth_mbps`/`rtt_ms` cannot overflow (and panic in debug / wrap in
    /// release); the result is range-checked against `VARINT_MAX` in `validate()`.
    pub fn target_bytes(&self) -> u64 {
        let bytes_per_sec = (self.bandwidth_mbps as u64).saturating_mul(1_000_000) / 8;
        bytes_per_sec.saturating_mul(self.rtt_ms as u64) / 1000
    }
}

/// Bulk result-transfer tuning (`[transport.result]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResultTransferConfig {
    /// Number of concurrent unidirectional streams used to transfer one large
    /// result. `1` = single inline stream (back-compatible). Bounded above by
    /// `transport.quic.max_concurrent_uni_streams`. Overridable per call.
    pub parallelism: usize,
    /// Size of each streamed result chunk (bytes). `None` inherits
    /// `network.result_chunk_bytes`. QUIC flow control backpressures on this.
    pub chunk_bytes: Option<usize>,
    /// Only split a result across multiple streams when its serialized size is at
    /// least this many bytes (avoids per-stream overhead for small results).
    pub parallel_min_bytes: usize,
    /// Hard cap (bytes) on a single **received** result payload. The winning
    /// worker is untrusted and the bulk transfer deliberately bypasses the
    /// control-frame cap, so the receiver refuses to allocate a reassembly buffer
    /// for any `ResultManifest` declaring more than this (additionally clamped to
    /// `p2p_proto::MAX_RESULT_BYTES`). Defends against a malicious manifest
    /// forcing an out-of-memory abort on the requester.
    pub max_result_bytes: u64,
    /// Hard cap on the number of parallel result streams the receiver will accept
    /// for one result (additionally clamped to `p2p_proto::MAX_RESULT_PARTS`).
    /// Bounds the inbound `accept_uni` loop against an attacker-supplied `parts`.
    pub max_result_parts: u32,
}

impl Default for ResultTransferConfig {
    fn default() -> Self {
        Self {
            parallelism: 1,
            chunk_bytes: None,
            parallel_min_bytes: 1024 * 1024,
            max_result_bytes: 2 * 1024 * 1024 * 1024,
            max_result_parts: 1024,
        }
    }
}

/// Wire compression algorithm for result data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgo {
    /// No compression (default; best on loopback/LAN where CPU is the bottleneck).
    None,
    /// LZ4 — very fast, modest ratio. Good default for WAN.
    Lz4,
    /// Zstd — higher ratio at a CPU cost; tune `level` for WAN links.
    Zstd,
}

/// Optional wire compression for result data (`[transport.compression]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    /// Algorithm: "none" | "lz4" | "zstd". Default off; see WAN guidance in docs.
    pub algorithm: CompressionAlgo,
    /// Codec level (zstd: 1..=22; ignored by lz4/none).
    pub level: i32,
    /// Only compress payloads at least this many bytes (skip tiny results).
    pub min_size_bytes: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            algorithm: CompressionAlgo::None,
            level: 3,
            min_size_bytes: 64 * 1024,
        }
    }
}

/// How the node's TLS identity verifies peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinningMode {
    /// Trust-on-first-use: accept any valid self-signed peer, record its node id.
    Tofu,
    /// Only accept peers whose node id is in the allowlist.
    Allowlist,
}

/// Node identity / certificate settings (architecture §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdentityConfig {
    /// Path to a PKCS#8 Ed25519 private key (PEM). If absent, a fresh ephemeral
    /// identity is generated (useful for tests).
    pub key_path: Option<String>,
    /// Peer verification mode.
    pub pinning_mode: PinningMode,
    /// Allowlisted peer node ids (used when `pinning_mode = allowlist`).
    pub allowlist: Vec<String>,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            key_path: None,
            pinning_mode: PinningMode::Tofu,
            allowlist: Vec::new(),
        }
    }
}

/// Discovery / membership mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryMode {
    /// Static seed list only (MVP).
    Static,
    /// Kademlia DHT + gossip (scales to thousands of hosts).
    Kademlia,
}

/// Discovery settings. Scales sub-linearly via DHT + candidate sampling
/// (architecture §8); never broadcasts to all peers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub mode: DiscoveryMode,
    /// Bootstrap peers used only to *enter* the swarm (never in the data path).
    ///
    /// For the `kademlia` mode these are libp2p multiaddrs that include the peer
    /// id, e.g. `/ip4/203.0.113.10/tcp/9595/p2p/12D3Koo...`.
    pub bootstrap: Vec<String>,
    /// libp2p listen multiaddrs for the discovery overlay (Kademlia + gossip).
    /// This is the *discovery* transport, distinct from the QUIC data-plane
    /// `network.bind_addr`. Empty = listen on an OS-assigned ephemeral TCP port
    /// on loopback (handy for tests). e.g. `["/ip4/0.0.0.0/tcp/9595"]`.
    pub listen_addrs: Vec<String>,
    /// Maximum number of candidate workers a requester will ever contact for a
    /// single job — bounds fan-out regardless of swarm size.
    pub candidate_sample_size: usize,
    pub kademlia: KademliaConfig,
    pub gossip: GossipConfig,
    /// Global NAT-traversal stack (identify + AutoNAT + DCUtR hole punching +
    /// Circuit Relay v2/AutoRelay + mDNS) so nodes behind home/office NATs in
    /// different networks can connect directly with no central server.
    pub nat: NatConfig,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mode: DiscoveryMode::Static,
            bootstrap: Vec::new(),
            listen_addrs: Vec::new(),
            candidate_sample_size: 16,
            kademlia: KademliaConfig::default(),
            gossip: GossipConfig::default(),
            nat: NatConfig::default(),
        }
    }
}

/// Kademlia DHT parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KademliaConfig {
    /// Replication factor (k-bucket size).
    pub replication_factor: usize,
    /// Query parallelism (alpha).
    pub query_parallelism: usize,
    /// Record TTL in the DHT (secs).
    pub record_ttl_secs: u64,
}

impl Default for KademliaConfig {
    fn default() -> Self {
        Self {
            replication_factor: 20,
            query_parallelism: 3,
            record_ttl_secs: 3600,
        }
    }
}

/// Gossip / pubsub parameters for capability ads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GossipConfig {
    /// Gossipsub topic that signed capability ads are published on. Carries the
    /// protocol major so cross-major swarms cannot share a mesh.
    pub topic: String,
    /// How often a worker republishes its capability record (ms). Also the
    /// gossipsub heartbeat interval.
    pub heartbeat_ms: u64,
    /// Mesh fanout (number of peers each message is forwarded to).
    pub fanout: usize,
    /// Capability record freshness window (secs) before a peer is considered
    /// stale. Ads older than this (or with a far-future ts) are rejected on
    /// receipt and excluded from candidate sampling.
    pub capability_ttl_secs: u64,
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            topic: "duckdb-p2p/caps/1".to_string(),
            heartbeat_ms: 5_000,
            fanout: 6,
            capability_ttl_secs: 30,
        }
    }
}

/// Global NAT-traversal stack for the libp2p discovery overlay (architecture
/// §8 "Networking & NAT traversal").
///
/// `identify` is always on (the overlay needs it to learn peer addresses and
/// observed external addresses). These knobs gate the *optional* behaviours that
/// let two nodes behind home/office NATs on different networks worldwide connect
/// **directly, with no central server and no fixed IP/URL**:
///
/// * **AutoNAT** — probe peers to learn whether we are publicly reachable and
///   discover our external address.
/// * **DCUtR** — coordinated hole punching to upgrade a relayed connection into
///   a *direct* one through NATs (works over QUIC/UDP). Requires `relay_client`.
/// * **Circuit Relay v2 client + AutoRelay** — when hole punching fails
///   (symmetric NAT), route through **volunteer relay peers** auto-selected from
///   the network (never a central server). `act_as_relay` lets this node *be* a
///   volunteer relay for others.
/// * **mDNS** — zero-config peer discovery on the same LAN.
///
/// Layers like everything else: defaults → TOML (`[discovery.nat]`) →
/// `P2P_DISCOVERY_NAT_*` env → per-call. Nothing is hard-coded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NatConfig {
    /// Enable AutoNAT reachability detection + external-address discovery.
    pub autonat: bool,
    /// Enable DCUtR hole punching (relay-assisted direct-connection upgrade).
    /// Requires `relay_client` (DCUtR coordinates over an existing relayed link).
    pub dcutr: bool,
    /// Enable the Circuit Relay v2 client + AutoRelay (reserve circuit slots on
    /// volunteer relays so unreachable peers can still be dialed).
    pub relay_client: bool,
    /// Act as a Circuit Relay v2 **server** — volunteer to relay traffic for
    /// other peers (subject to `relay_limits`). Off by default.
    pub act_as_relay: bool,
    /// Enable mDNS zero-config discovery of peers on the local network.
    pub mdns: bool,
    /// How often mDNS re-queries the LAN for peers (secs). Lower = snappier LAN
    /// discovery + recovery from a lost initial packet, at the cost of a little
    /// multicast traffic. Only used when `mdns = true`.
    pub mdns_query_interval_secs: u64,
    /// Explicit externally-reachable multiaddrs to advertise to peers (augments
    /// any AutoNAT-discovered address). e.g.
    /// `["/ip4/203.0.113.10/udp/9595/quic-v1"]`.
    pub external_addresses: Vec<String>,
    /// Known relay multiaddrs (including `/p2p/<peer-id>`) to reserve a circuit
    /// slot with on startup. Empty = AutoRelay auto-selects relays discovered
    /// from the network (no central directory).
    pub relays: Vec<String>,
    /// Maximum number of relays to hold reservations with simultaneously
    /// (AutoRelay fan-out cap). Bounds resource use regardless of swarm size.
    pub max_relays: usize,
    /// Limits enforced when `act_as_relay = true`.
    pub relay_limits: RelayLimitsConfig,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            autonat: true,
            dcutr: true,
            relay_client: true,
            act_as_relay: false,
            mdns: true,
            mdns_query_interval_secs: 300,
            external_addresses: Vec::new(),
            relays: Vec::new(),
            max_relays: 3,
            relay_limits: RelayLimitsConfig::default(),
        }
    }
}

/// Resource limits applied when a node volunteers as a Circuit Relay v2 server
/// (`discovery.nat.act_as_relay = true`). Mirror of `libp2p::relay::Config`'s
/// caps so a volunteer relay cannot be abused into unbounded resource use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelayLimitsConfig {
    /// Max concurrent reservations across all peers.
    pub max_reservations: usize,
    /// Max concurrent reservations from a single peer.
    pub max_reservations_per_peer: usize,
    /// How long a granted reservation lasts (secs).
    pub reservation_duration_secs: u64,
    /// Max concurrent relayed circuits across all peers.
    pub max_circuits: usize,
    /// Max concurrent relayed circuits from a single source peer.
    pub max_circuits_per_peer: usize,
    /// Max duration of a single relayed circuit (secs).
    pub max_circuit_duration_secs: u64,
    /// Max bytes relayed over a single circuit before it is closed.
    pub max_circuit_bytes: u64,
}

impl Default for RelayLimitsConfig {
    fn default() -> Self {
        // Matches libp2p::relay::Config defaults (safe, bounded volunteer relay).
        Self {
            max_reservations: 128,
            max_reservations_per_peer: 4,
            reservation_duration_secs: 60 * 60,
            max_circuits: 16,
            max_circuits_per_peer: 4,
            max_circuit_duration_secs: 2 * 60,
            max_circuit_bytes: 1 << 17,
        }
    }
}

/// Hedged-execution scheduler settings (requester side, architecture §11) plus
/// the **resilience / re-dispatch** layer (architecture §8/§11): per-attempt
/// deadline, unlimited-by-default retries with bounded exponential backoff +
/// jitter, a global retry/hedge token-bucket budget, a wall-clock cap, and the
/// progress-stall (streamed-heartbeat) liveness timeout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Number of workers to race (`k`). Must be >= `quorum`.
    pub replicas: usize,
    /// Number of matching result hashes required to accept (`q`).
    pub quorum: usize,
    /// Verification mode: "fast" or "quorum".
    pub verify_mode: VerifyModeCfg,
    /// Timeout to collect bids after sending offers (ms).
    pub offer_timeout_ms: u64,
    /// Timeout for a dispatched job to commit a result hash (ms).
    pub dispatch_timeout_ms: u64,
    /// Requester-side per-attempt deadline (ms): the wall-clock budget for ONE
    /// dispatch attempt (offer → dispatch → commit). An attempt whose workers go
    /// silent past this is treated as **inconclusive (job-fault, no provider
    /// penalty)** and re-dispatched to a fresh candidate set.
    pub attempt_deadline_ms: u64,
    /// Maximum number of (re)dispatch attempts. **`0` = unlimited** (the
    /// default): keep routing a stalled/failed job to fresh nodes until it
    /// completes, bounded only by the backoff, the retry budget, and
    /// `max_total_duration_ms`.
    pub max_retries: u32,
    /// Optional wall-clock cap (ms) on the whole resilient re-dispatch loop.
    /// `0` = no cap. When set, the loop stops re-dispatching once exceeded.
    pub max_total_duration_ms: u64,
    /// Initial retry backoff (ms); doubles up to `backoff_max_ms`.
    pub backoff_initial_ms: u64,
    pub backoff_max_ms: u64,
    /// Jitter fraction `[0,1]` applied to each backoff delay (full jitter at
    /// `1.0`): the actual sleep is uniformly sampled in
    /// `[base*(1-frac), base]` to de-synchronize retry storms across requesters.
    pub backoff_jitter_frac: f64,
    /// Global retry/hedge **token bucket**: maximum tokens (burst capacity). Each
    /// re-dispatch attempt past the first costs one token; when the bucket is
    /// empty the loop stops re-dispatching (prevents retry storms). `0` =
    /// unlimited (no budget enforcement).
    pub retry_budget_max_tokens: f64,
    /// Token-bucket refill rate (tokens per second).
    pub retry_budget_refill_per_sec: f64,
    /// Requester-side expected progress/heartbeat interval (ms) — what the
    /// requester assumes the host streams progress at. Combined with
    /// `progress_stall_multiplier` to derive the stall timeout.
    pub progress_interval_ms: u64,
    /// Stall timeout multiplier: an attempt is declared **stalled** (and
    /// re-dispatched) if no progress/heartbeat arrives within
    /// `progress_interval_ms * progress_stall_multiplier`. "Several × the report
    /// interval" per the design.
    pub progress_stall_multiplier: u32,
    /// Maximum number of jobs a requester runs concurrently (semaphore bound).
    pub max_inflight_jobs: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            replicas: 3,
            quorum: 2,
            verify_mode: VerifyModeCfg::Quorum,
            offer_timeout_ms: 2_000,
            dispatch_timeout_ms: 30_000,
            attempt_deadline_ms: 60_000,
            max_retries: 0,
            max_total_duration_ms: 0,
            backoff_initial_ms: 200,
            backoff_max_ms: 5_000,
            backoff_jitter_frac: 0.5,
            retry_budget_max_tokens: 32.0,
            retry_budget_refill_per_sec: 4.0,
            progress_interval_ms: 2_000,
            progress_stall_multiplier: 5,
            max_inflight_jobs: 64,
        }
    }
}

impl SchedulerConfig {
    /// The progress-stall timeout in milliseconds
    /// (`progress_interval_ms * progress_stall_multiplier`). A non-positive
    /// product disables stall detection (returns `0`).
    pub fn stall_timeout_ms(&self) -> u64 {
        self.progress_interval_ms
            .saturating_mul(self.progress_stall_multiplier as u64)
    }
}

/// Host (worker) execution-deadline + progress-streaming settings (architecture
/// §11 resilience). The host abandons a job that exceeds `job_timeout_ms`, and
/// streams a progress/heartbeat update every `progress_interval_ms` while it
/// runs (the progress update IS the liveness signal the requester watches).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    /// Host execution deadline (ms): a job running longer than this is
    /// **abandoned** by the host (the requester then re-dispatches). `0` =
    /// no host-side deadline.
    pub job_timeout_ms: u64,
    /// How often the host streams a progress/heartbeat update to the requester
    /// while a job executes (ms). `0` disables progress streaming.
    pub progress_interval_ms: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            job_timeout_ms: 60_000,
            progress_interval_ms: 2_000,
        }
    }
}

/// Liveness / failure-detection settings (architecture §8): a **phi-accrual**
/// failure detector over heartbeat/gossip intervals plus **SWIM-style indirect
/// probing**, layered on the libp2p gossip overlay. Unhealthy/suspect peers are
/// excluded from candidate selection. Off-path by default: the detector only
/// affects selection once a [`crate`]-level liveness view is wired into the
/// coordinator, so a node with no liveness wiring behaves exactly as before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LivenessConfig {
    pub phi: PhiAccrualConfig,
    pub swim: SwimConfig,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            phi: PhiAccrualConfig::default(),
            swim: SwimConfig::default(),
        }
    }
}

/// Phi-accrual failure detector (φ = -log10(P_late) over a sliding window of
/// heartbeat intervals). A peer is convicted (suspected dead) once φ exceeds
/// `convict_threshold` (~8–12 typical).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhiAccrualConfig {
    /// Enable the phi-accrual detector.
    pub enabled: bool,
    /// φ value at/above which a peer is considered dead. Higher = more
    /// conservative (fewer false positives). Typical 8–12.
    pub convict_threshold: f64,
    /// Sliding-window size: number of recent heartbeat intervals retained.
    pub window_size: usize,
    /// Floor on the interval standard deviation (ms) to avoid over-confidence
    /// when arrivals are very regular (prevents φ from spiking on tiny jitter).
    pub min_std_ms: f64,
    /// Extra slack (ms) added to the estimated mean interval — tolerates a
    /// known acceptable pause (e.g. GC) without convicting.
    pub acceptable_pause_ms: f64,
    /// Bootstrap interval estimate (ms) used before enough samples accumulate
    /// (also the assumed first inter-arrival).
    pub first_interval_ms: f64,
}

impl Default for PhiAccrualConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            convict_threshold: 8.0,
            window_size: 100,
            min_std_ms: 50.0,
            acceptable_pause_ms: 0.0,
            first_interval_ms: 5_000.0,
        }
    }
}

/// SWIM-style indirect probing: before declaring a peer dead, ask `k` random
/// peers to probe it (reduces false positives from a single bad link).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwimConfig {
    /// Enable SWIM indirect probing.
    pub enabled: bool,
    /// `k` — number of random peers asked to indirect-probe a suspect.
    pub indirect_probe_count: usize,
    /// Per-probe timeout (ms) for a direct probe.
    pub probe_timeout_ms: u64,
    /// Per-probe timeout (ms) for each indirect (relayed) probe.
    pub indirect_probe_timeout_ms: u64,
}

impl Default for SwimConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            indirect_probe_count: 3,
            probe_timeout_ms: 1_000,
            indirect_probe_timeout_ms: 2_000,
        }
    }
}

/// Mirror of `p2p_proto::VerifyMode` for config files (kept separate so config
/// does not force a proto dependency on consumers that only want config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyModeCfg {
    Fast,
    Quorum,
}

/// Worker resource donation + admission control (architecture §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// Total memory the host donates (bytes).
    pub memory_bytes: u64,
    /// Total threads the host donates.
    pub threads: u32,
    /// Max concurrent jobs the host will admit.
    pub max_jobs: u32,
    /// Per-job default memory lease (bytes) if a dispatch doesn't specify.
    pub per_job_memory_bytes: u64,
    /// Per-job default thread lease.
    pub per_job_threads: u32,
    /// Data classes the host is willing to serve.
    pub data_classes: Vec<DataClassCfg>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            memory_bytes: 4 * 1024 * 1024 * 1024,
            threads: 2,
            max_jobs: 3,
            per_job_memory_bytes: 1024 * 1024 * 1024,
            per_job_threads: 1,
            data_classes: vec![DataClassCfg::Public],
        }
    }
}

/// Mirror of `p2p_proto::DataClass` for config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClassCfg {
    Public,
    Internal,
    Sensitive,
}

/// Trust engine settings (architecture §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrustConfig {
    /// Minimum soft trust score `[0,1]` a worker needs to be selected.
    pub min_trust: f64,
    /// Minimum attestation level: "L0" | "L1" | "L2" (hard gate).
    pub min_attestation: String,
    /// Half-life of reputation recency weighting (secs).
    pub reputation_half_life_secs: u64,
    /// Soft-score weights.
    pub weights: ReputationWeights,
    /// Probability `[0,1]` a dispatched job is a canary with a known answer.
    pub canary_rate: f64,
    /// Penalty subtracted from a worker's score on an incorrect verdict.
    pub incorrect_penalty: f64,
    /// Trust assigned to brand-new identities with no history.
    pub bootstrap_trust: f64,
    /// Optional path to a persistent (embedded `redb`) trust-store database.
    /// `None` (default) keeps the bounded in-memory store, so reputation/receipts
    /// do not survive a restart. Set a path on long-lived nodes to persist the
    /// reputation trail across restarts.
    pub store_path: Option<String>,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            min_trust: 0.7,
            min_attestation: "L0".to_string(),
            reputation_half_life_secs: 7 * 24 * 3600,
            weights: ReputationWeights::default(),
            canary_rate: 0.05,
            incorrect_penalty: 0.5,
            bootstrap_trust: 0.1,
            store_path: None,
        }
    }
}

/// Weights for `effective_trust` (architecture §7.5):
/// `α·R + β·age + γ·voucher + δ·stake − penalties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReputationWeights {
    pub alpha_reputation: f64,
    pub beta_age: f64,
    pub gamma_voucher: f64,
    pub delta_stake: f64,
}

impl Default for ReputationWeights {
    fn default() -> Self {
        Self {
            alpha_reputation: 0.7,
            beta_age: 0.1,
            gamma_voucher: 0.1,
            delta_stake: 0.1,
        }
    }
}

/// Sybil-resistance settings (architecture §7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SybilConfig {
    /// Number of leading zero bits a new identity's PoW must satisfy.
    pub pow_difficulty_bits: u32,
    /// Minimum stake/deposit required to mint an identity (abstract units).
    pub min_stake: u64,
    /// Trust weight granted per voucher signature.
    pub vouch_weight: f64,
}

impl Default for SybilConfig {
    fn default() -> Self {
        Self {
            pow_difficulty_bits: 16,
            min_stake: 0,
            vouch_weight: 0.05,
        }
    }
}

/// Object-storage / credential settings (architecture §9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Default/primary credential-provider id: "local-fake" | "s3" | "az" | "gcs".
    pub provider: String,
    /// Endpoint URL (used by local fake or S3-compatible / MinIO stores), e.g.
    /// `minio.local:9000`. Top-level default; per-provider overrides live in
    /// `[storage.provider_options.<id>]`.
    pub endpoint: Option<String>,
    /// Region (cloud providers / S3-compatible stores).
    pub region: Option<String>,
    /// S3 URL addressing style for S3-compatible / MinIO endpoints: `"path"`
    /// (MinIO and most self-hosted stores) or `"vhost"` (AWS default). Maps to
    /// the DuckDB s3 secret `URL_STYLE`. Top-level default; overridable per
    /// provider via `[storage.provider_options.s3] url_style = "..."`.
    pub url_style: Option<String>,
    /// Whether S3-compatible endpoints use TLS (maps to DuckDB `USE_SSL`). MinIO
    /// dev deployments are often plain HTTP (`false`). Top-level default;
    /// overridable per provider via `[storage.provider_options.s3] use_ssl`.
    pub use_ssl: Option<bool>,
    /// TTL of issued scoped credentials (secs).
    pub credential_ttl_secs: u64,
    /// TTL of per-job sealed data keys (secs).
    pub key_ttl_secs: u64,

    // --- Data-source / secure-read knobs (architecture §4, §9.2, §9.4) -------
    /// Master switch: allow worker network egress for remote object-storage
    /// reads. When `false`, the execution engine stays in the strict/local
    /// lockdown (`enable_external_access=false`) and cannot reach the network.
    /// Enabling this requires complementary OS-level egress filtering (deferred,
    /// architecture §9.4) — DuckDB cannot restrict egress to specific endpoints.
    pub enable_remote_access: bool,
    /// Fail engine init if a `preload_extensions` entry cannot be loaded
    /// (extensions are pre-loaded at init, never `INSTALL`/`LOAD` at query time).
    pub require_extensions: bool,
    /// DuckDB extensions pre-loaded at engine init (e.g. "httpfs", "aws",
    /// "azure", "parquet", "json", "delta", "iceberg"). Explicit & configurable.
    pub preload_extensions: Vec<String>,
    /// Formats workers will serve (e.g. "csv", "json", "parquet", "delta",
    /// "iceberg"). Extensible — unknown values are accepted for forward-compat.
    pub enabled_formats: Vec<String>,
    /// Storage/credential providers enabled on this node.
    pub enabled_providers: Vec<String>,
    /// Local directories DuckDB may read even with external access disabled
    /// (maps to DuckDB `allowed_directories`). Used for local fixtures/tests.
    pub allowed_local_paths: Vec<String>,
    /// Per-provider option overrides, keyed by provider id (e.g.
    /// `[storage.provider_options.s3] endpoint = "..."` / `region = "..."`).
    pub provider_options: BTreeMap<String, BTreeMap<String, String>>,
    /// Per-format reader option overrides, keyed by format id.
    pub format_options: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            provider: "local-fake".to_string(),
            endpoint: None,
            region: None,
            url_style: None,
            use_ssl: None,
            credential_ttl_secs: 900,
            key_ttl_secs: 900,
            enable_remote_access: false,
            require_extensions: true,
            preload_extensions: Vec::new(),
            enabled_formats: vec!["csv".to_string(), "json".to_string(), "parquet".to_string()],
            enabled_providers: vec!["local-fake".to_string()],
            allowed_local_paths: Vec::new(),
            provider_options: BTreeMap::new(),
            format_options: BTreeMap::new(),
        }
    }
}

/// Where a query should run: locally (free, in-process), on the grid (paid,
/// hedged/quorum), or `auto` (let the planner decide from a pre-flight estimate
/// vs. local headroom). Mirrors the per-call `prefer => local|remote|auto`
/// argument to `p2p_query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferMode {
    /// Always run locally and free (you trust your own machine). No bidding,
    /// escrow, quorum or payment.
    Local,
    /// Always dispatch to the grid (hedged, quorum-verified).
    Remote,
    /// Decide per query from the data-size estimate vs. current local headroom.
    Auto,
}

/// Local-first execution planner (architecture §4 data plane, §11 scheduler).
///
/// A node may run a query entirely in its own locked-down in-process DuckDB
/// with NO bidding / escrow / quorum / payment (it trusts its own machine).
/// This section governs **when** that free local path is chosen over a grid
/// dispatch: a pre-flight, metadata-only data-size estimate is translated into
/// an estimated peak working-set memory and compared against the node's CURRENT
/// available headroom (`budget.memory_bytes * ram_fraction` minus memory already
/// in use by concurrent local jobs), subject to a spill tolerance and a latency
/// budget. If a job started locally blows past the threshold mid-flight it is
/// aborted and re-dispatched to the grid (adaptive fail-over).
///
/// Nothing here is hard-coded; every knob layers defaults → TOML →
/// `P2P_PLANNER_*` env → per-call (`prefer`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlannerConfig {
    /// Master switch. When `false` the planner is bypassed and every query is
    /// dispatched to the grid exactly as before (back-compatible).
    pub enabled: bool,
    /// Whether this node is allowed to execute queries on its **own** machine at
    /// all. `true` (default) keeps the local-first behavior. When `false` the
    /// node runs in **remote-only ("route everything to the grid") mode**: every
    /// query — even a tiny one that would otherwise fit locally — is dispatched
    /// to the grid, the adaptive fail-over's "start local" path is skipped
    /// entirely, and a node that never called `p2p_share` operates as a pure
    /// thin-client requester. This is a hard gate that overrides `prefer`
    /// (including a per-call `prefer => 'local'`). With no reachable hosts a
    /// query returns a clear `NoCandidates` error rather than falling back to
    /// local.
    pub local_execution_enabled: bool,
    /// Default routing preference when a call does not override `prefer`. Set to
    /// `remote` for a sticky "prefer the grid" default (still allows a per-call
    /// `prefer => 'local'` override unless `local_execution_enabled = false`).
    pub prefer: PreferMode,
    /// Fraction `alpha` in `(0,1]` of `budget.memory_bytes` the node is willing
    /// to devote to local execution. The local headroom is
    /// `budget.memory_bytes * ram_fraction - in_use_by_local_jobs`.
    pub ram_fraction: f64,
    /// Max number of free local jobs that may run concurrently. When all slots
    /// are taken the planner routes to the grid (locally-saturated → remote).
    pub max_concurrent_local_jobs: usize,
    /// Absolute cap on estimated scanned (uncompressed) input bytes for the
    /// local path. A query whose estimate exceeds this always goes to the grid,
    /// regardless of headroom (protects against pathological scans).
    pub size_threshold_bytes: u64,
    /// How much the estimated peak working set may exceed current RAM headroom
    /// and still run locally, relying on DuckDB's out-of-core spill to disk.
    /// `0` = never spill (must fit in RAM headroom).
    pub spill_tolerance_bytes: u64,
    /// Latency budget for the local path (ms). If the estimated local runtime
    /// exceeds this the planner prefers the (parallel, hedged) grid. `0`
    /// disables the latency gate.
    pub max_local_latency_ms: u64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            local_execution_enabled: true,
            prefer: PreferMode::Auto,
            ram_fraction: 0.6,
            max_concurrent_local_jobs: 4,
            size_threshold_bytes: 256 * 1024 * 1024,
            spill_tolerance_bytes: 512 * 1024 * 1024,
            max_local_latency_ms: 10_000,
        }
    }
}

/// Bounded caches & concurrency (scalability: no unbounded maps).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Max receipts retained per worker in the in-memory reputation store.
    pub receipt_cache_per_worker: usize,
    /// Max number of distinct workers tracked in the trust store (LRU evicted).
    pub trust_store_capacity: usize,
    /// Max number of peers held in the discovery peer cache (LRU evicted).
    pub peer_cache_capacity: usize,
    /// Max concurrent inbound jobs processed by the worker pool (semaphore).
    pub worker_pool_size: usize,
    /// Max connections retained in the connection pool.
    pub connection_pool_size: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            receipt_cache_per_worker: 256,
            trust_store_capacity: 100_000,
            peer_cache_capacity: 50_000,
            worker_pool_size: 8,
            connection_pool_size: 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// OS-level execution sandbox (`[sandbox]`) — architecture §9.4
// ---------------------------------------------------------------------------

/// Which OS sandbox backend to apply around job execution. **OS-agnostic**: the
/// abstraction is platform-independent for callers; each concrete backend is
/// `#[cfg]`-gated in `p2p_node::sandbox` so the workspace compiles on every
/// target, degrading to a no-op anywhere unsupported.
///
/// DuckDB's own lockdown (`enable_external_access`, `lock_configuration`,
/// `allowed_directories`, ephemeral temp) is always present; this selects the
/// *complementary* OS-level boundary. `Auto` resolves to the best backend for
/// the current platform at runtime; `None` is an explicit no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxBackend {
    /// Pick the best available backend for the host platform at runtime
    /// (Linux→cgroups+seccomp+egress; macOS→Seatbelt; Windows→Job Objects;
    /// Android→app sandbox + seccomp/cgroup; iOS→app sandbox in-process only;
    /// other Unix→rlimit; everything else→no-op).
    #[serde(rename = "auto")]
    Auto,
    /// Explicit no-op (today's behavior) regardless of platform.
    #[serde(rename = "none")]
    None,
    /// Portable POSIX `setrlimit` resource caps (RAM/CPU/FD/file-size). Unix
    /// only; there is no `rlimit` on Windows (use `windows-jobobject` there).
    #[serde(rename = "rlimit")]
    Rlimit,
    /// Linux cgroups v2 (memory/CPU caps) + seccomp-bpf (syscall filtering) +
    /// network-namespace / egress allow-list. Policy builders are portable +
    /// unit-tested; enforcement requires a Linux host.
    #[serde(rename = "cgroups-seccomp")]
    CgroupsSeccomp,
    /// macOS Seatbelt (`sandbox-exec`/`sandbox_init`) filesystem + network
    /// egress profile.
    #[serde(rename = "macos-seatbelt")]
    MacosSeatbelt,
    /// Windows Job Objects (memory/CPU/active-process caps, kill-on-close) +
    /// restricted-token/AppContainer isolation + WFP/firewall egress rules.
    #[serde(rename = "windows-jobobject")]
    WindowsJobObject,
    /// Android: relies on the platform app sandbox + SELinux, adding seccomp-bpf
    /// + app-scoped cgroup limits where available. A constrained host.
    #[serde(rename = "android")]
    Android,
    /// iOS: relies on the OS app sandbox; **cannot spawn subprocesses**, so only
    /// in-process resource accounting applies. Realistically a client/requester
    /// or a light in-process host, not a general multi-job compute host.
    #[serde(rename = "ios")]
    Ios,
}

/// How the per-resource limits are determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLimitsMode {
    /// Derive limits from the donated `[budget]` (memory/threads/max_jobs).
    InheritBudget,
    /// Use the explicit per-resource limits below (a `0` field = unlimited).
    Explicit,
}

/// How the network egress allow-list is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEgressMode {
    /// Derive the allow-list from the configured `[storage]` providers /
    /// endpoints, so a job can reach object storage but nothing else.
    InheritStorage,
    /// Use only the explicit `egress_allowlist` host[:port] entries below.
    Explicit,
}

/// Policy for the job's temp/scratch directory under the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTempDirPolicy {
    /// A fresh per-job ephemeral directory (created + wiped by the runtime).
    Ephemeral,
    /// Inherit the process temp dir (no extra scoping).
    Inherit,
    /// Use the explicit `temp_dir` path below.
    Custom,
}

/// Per-resource OS limits applied to job execution. A field set to `0` means
/// "unlimited / inherit" (no cap installed for that resource).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxLimitsConfig {
    /// How limits are resolved: from the donated budget or explicit values.
    pub mode: SandboxLimitsMode,
    /// Address-space / RAM cap in bytes (`RLIMIT_AS`). Used only in `explicit`
    /// mode; in `inherit_budget` mode the per-job budget memory is used.
    pub memory_bytes: u64,
    /// CPU-time cap in seconds (`RLIMIT_CPU`); `0` = unlimited.
    pub cpu_seconds: u64,
    /// Maximum size of any file the job may create in bytes (`RLIMIT_FSIZE`);
    /// `0` = unlimited.
    pub max_file_size_bytes: u64,
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`); `0` = inherit.
    pub max_open_files: u64,
    /// Maximum number of processes/threads (`RLIMIT_NPROC`); `0` = inherit. Used
    /// only in `explicit` mode (budget mode derives it from threads).
    pub max_processes: u64,
}

impl Default for SandboxLimitsConfig {
    fn default() -> Self {
        Self {
            mode: SandboxLimitsMode::InheritBudget,
            memory_bytes: 0,
            cpu_seconds: 0,
            max_file_size_bytes: 0,
            max_open_files: 0,
            max_processes: 0,
        }
    }
}

/// OS-level execution sandbox configuration (`[sandbox]`, architecture §9.4).
///
/// Layers like everything else: defaults → TOML → `P2P_SANDBOX_*` env →
/// per-call. Off by default so a node behaves exactly as today (no-op sandbox)
/// until an operator opts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Master switch. `false` = the [`SandboxBackend::None`] no-op (today's
    /// behavior); job execution is wrapped in nothing extra.
    pub enabled: bool,
    /// Which backend to apply when enabled.
    pub backend: SandboxBackend,
    /// Resource caps (RAM/CPU/FD/file-size) applied via rlimit.
    pub limits: SandboxLimitsConfig,
    /// How the network egress allow-list is derived.
    pub egress_mode: SandboxEgressMode,
    /// Explicit egress allow-list (`host` or `host:port`) used when
    /// `egress_mode = explicit`. In `inherit_storage` mode this is *appended*
    /// to the storage-derived endpoints.
    pub egress_allowlist: Vec<String>,
    /// Temp/scratch directory policy for sandboxed jobs.
    pub temp_dir_policy: SandboxTempDirPolicy,
    /// Explicit temp dir, required when `temp_dir_policy = custom`.
    pub temp_dir: Option<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: SandboxBackend::Auto,
            limits: SandboxLimitsConfig::default(),
            egress_mode: SandboxEgressMode::InheritStorage,
            egress_allowlist: Vec::new(),
            temp_dir_policy: SandboxTempDirPolicy::Ephemeral,
            temp_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading & validation
// ---------------------------------------------------------------------------

impl GridConfig {
    /// Load config applying layers: defaults → file → environment.
    ///
    /// `file` is optional; if `None`, the `P2P_CONFIG` env var is consulted for
    /// a path. Per-call SQL overrides are applied separately by the caller.
    pub fn load(file: Option<&Path>) -> Result<Self, ConfigError> {
        let env_path = std::env::var("P2P_CONFIG").ok();
        let mut cfg = match (file, env_path.as_deref()) {
            (Some(p), _) => Self::from_toml_file(p)?,
            (None, Some(p)) => Self::from_toml_file(Path::new(p))?,
            (None, None) => Self::default(),
        };
        cfg.apply_env()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse a TOML file on top of defaults (missing fields keep their default).
    pub fn from_toml_file(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(path.display().to_string(), e))?;
        Self::from_toml_str(&text)
    }

    /// Parse TOML text on top of defaults.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Serialize the (fully-resolved) config back to TOML — used to emit the
    /// effective configuration for operators.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("GridConfig is always serializable")
    }

    /// Apply environment-variable overrides. Documented keys (highest of the
    /// non-per-call layers). Each parse failure is reported with its var name.
    pub fn apply_env(&mut self) -> Result<(), ConfigError> {
        let env: BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("P2P_"))
            .collect();
        self.apply_env_map(&env)
    }

    /// Testable core of [`apply_env`] operating on an explicit map.
    pub fn apply_env_map(&mut self, env: &BTreeMap<String, String>) -> Result<(), ConfigError> {
        fn parse<T: std::str::FromStr>(k: &str, v: &str) -> Result<T, ConfigError>
        where
            T::Err: std::fmt::Display,
        {
            v.parse::<T>()
                .map_err(|e| ConfigError::Env(k.to_string(), e.to_string()))
        }

        for (k, v) in env {
            match k.as_str() {
                "P2P_BIND_ADDR" => self.network.bind_addr = v.clone(),
                "P2P_ADVERTISED_ADDR" => self.network.advertised_addr = Some(v.clone()),
                "P2P_IDLE_TIMEOUT_MS" => self.network.idle_timeout_ms = parse(k, v)?,
                "P2P_CONNECT_TIMEOUT_MS" => self.network.connect_timeout_ms = parse(k, v)?,
                "P2P_TRANSPORT_GSO" => self.transport.quic.gso = parse(k, v)?,
                "P2P_TRANSPORT_CONGESTION" => {
                    self.transport.quic.congestion = match v.as_str() {
                        "bbr" => CongestionAlgo::Bbr,
                        "cubic" => CongestionAlgo::Cubic,
                        "newreno" => CongestionAlgo::NewReno,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown congestion controller {other} (bbr|cubic|newreno)"),
                            ))
                        }
                    }
                }
                "P2P_TRANSPORT_ENABLE_0RTT" => self.transport.quic.enable_0rtt = parse(k, v)?,
                "P2P_TRANSPORT_RESULT_PARALLELISM" => {
                    self.transport.result.parallelism = parse(k, v)?
                }
                "P2P_TRANSPORT_COMPRESSION" => {
                    self.transport.compression.algorithm = match v.as_str() {
                        "none" => CompressionAlgo::None,
                        "lz4" => CompressionAlgo::Lz4,
                        "zstd" => CompressionAlgo::Zstd,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown compression algorithm {other} (none|lz4|zstd)"),
                            ))
                        }
                    }
                }
                "P2P_DISCOVERY_MODE" => {
                    self.discovery.mode = match v.as_str() {
                        "static" => DiscoveryMode::Static,
                        "kademlia" => DiscoveryMode::Kademlia,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown discovery mode {other}"),
                            ))
                        }
                    }
                }
                "P2P_BOOTSTRAP" | "P2P_DISCOVERY_BOOTSTRAP" => {
                    self.discovery.bootstrap =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_DISCOVERY_LISTEN_ADDRS" => {
                    self.discovery.listen_addrs =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_DISCOVERY_GOSSIP_TOPIC" => self.discovery.gossip.topic = v.clone(),
                "P2P_DISCOVERY_GOSSIP_HEARTBEAT_MS" => {
                    self.discovery.gossip.heartbeat_ms = parse(k, v)?
                }
                "P2P_DISCOVERY_GOSSIP_FANOUT" => self.discovery.gossip.fanout = parse(k, v)?,
                "P2P_DISCOVERY_CAPABILITY_TTL_SECS" => {
                    self.discovery.gossip.capability_ttl_secs = parse(k, v)?
                }
                "P2P_DISCOVERY_KAD_REPLICATION" => {
                    self.discovery.kademlia.replication_factor = parse(k, v)?
                }
                "P2P_DISCOVERY_KAD_QUERY_PARALLELISM" => {
                    self.discovery.kademlia.query_parallelism = parse(k, v)?
                }
                "P2P_DISCOVERY_KAD_RECORD_TTL_SECS" => {
                    self.discovery.kademlia.record_ttl_secs = parse(k, v)?
                }
                "P2P_CANDIDATE_SAMPLE_SIZE" => self.discovery.candidate_sample_size = parse(k, v)?,
                // ---- NAT traversal (env layer) ----
                "P2P_DISCOVERY_NAT_AUTONAT" => self.discovery.nat.autonat = parse(k, v)?,
                "P2P_DISCOVERY_NAT_DCUTR" => self.discovery.nat.dcutr = parse(k, v)?,
                "P2P_DISCOVERY_NAT_RELAY_CLIENT" => self.discovery.nat.relay_client = parse(k, v)?,
                "P2P_DISCOVERY_NAT_ACT_AS_RELAY" => self.discovery.nat.act_as_relay = parse(k, v)?,
                "P2P_DISCOVERY_NAT_MDNS" => self.discovery.nat.mdns = parse(k, v)?,
                "P2P_DISCOVERY_NAT_EXTERNAL_ADDRESSES" => {
                    self.discovery.nat.external_addresses =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_DISCOVERY_NAT_RELAYS" => {
                    self.discovery.nat.relays =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_DISCOVERY_NAT_MAX_RELAYS" => self.discovery.nat.max_relays = parse(k, v)?,
                "P2P_TRUST_STORE_PATH" => self.trust.store_path = Some(v.clone()),
                "P2P_REPLICAS" => self.scheduler.replicas = parse(k, v)?,
                "P2P_QUORUM" => self.scheduler.quorum = parse(k, v)?,
                "P2P_MAX_INFLIGHT_JOBS" => self.scheduler.max_inflight_jobs = parse(k, v)?,
                // ---- resilience / re-dispatch (env layer) ----
                "P2P_SCHEDULER_ATTEMPT_DEADLINE_MS" => {
                    self.scheduler.attempt_deadline_ms = parse(k, v)?
                }
                "P2P_SCHEDULER_MAX_RETRIES" | "P2P_MAX_RETRIES" => {
                    self.scheduler.max_retries = parse(k, v)?
                }
                "P2P_SCHEDULER_MAX_TOTAL_DURATION_MS" => {
                    self.scheduler.max_total_duration_ms = parse(k, v)?
                }
                "P2P_SCHEDULER_BACKOFF_INITIAL_MS" => {
                    self.scheduler.backoff_initial_ms = parse(k, v)?
                }
                "P2P_SCHEDULER_BACKOFF_MAX_MS" => self.scheduler.backoff_max_ms = parse(k, v)?,
                "P2P_SCHEDULER_BACKOFF_JITTER_FRAC" => {
                    self.scheduler.backoff_jitter_frac = parse(k, v)?
                }
                "P2P_SCHEDULER_RETRY_BUDGET_MAX_TOKENS" => {
                    self.scheduler.retry_budget_max_tokens = parse(k, v)?
                }
                "P2P_SCHEDULER_RETRY_BUDGET_REFILL_PER_SEC" => {
                    self.scheduler.retry_budget_refill_per_sec = parse(k, v)?
                }
                "P2P_SCHEDULER_PROGRESS_INTERVAL_MS" => {
                    self.scheduler.progress_interval_ms = parse(k, v)?
                }
                "P2P_SCHEDULER_PROGRESS_STALL_MULTIPLIER" => {
                    self.scheduler.progress_stall_multiplier = parse(k, v)?
                }
                // ---- worker execution deadline / progress (env layer) ----
                "P2P_WORKER_JOB_TIMEOUT_MS" => self.worker.job_timeout_ms = parse(k, v)?,
                "P2P_WORKER_PROGRESS_INTERVAL_MS" => {
                    self.worker.progress_interval_ms = parse(k, v)?
                }
                // ---- liveness: phi-accrual + SWIM (env layer) ----
                "P2P_LIVENESS_PHI_ENABLED" => self.liveness.phi.enabled = parse(k, v)?,
                "P2P_LIVENESS_PHI_CONVICT_THRESHOLD" => {
                    self.liveness.phi.convict_threshold = parse(k, v)?
                }
                "P2P_LIVENESS_PHI_WINDOW_SIZE" => self.liveness.phi.window_size = parse(k, v)?,
                "P2P_LIVENESS_PHI_MIN_STD_MS" => self.liveness.phi.min_std_ms = parse(k, v)?,
                "P2P_LIVENESS_PHI_ACCEPTABLE_PAUSE_MS" => {
                    self.liveness.phi.acceptable_pause_ms = parse(k, v)?
                }
                "P2P_LIVENESS_PHI_FIRST_INTERVAL_MS" => {
                    self.liveness.phi.first_interval_ms = parse(k, v)?
                }
                "P2P_LIVENESS_SWIM_ENABLED" => self.liveness.swim.enabled = parse(k, v)?,
                "P2P_LIVENESS_SWIM_INDIRECT_PROBE_COUNT" => {
                    self.liveness.swim.indirect_probe_count = parse(k, v)?
                }
                "P2P_LIVENESS_SWIM_PROBE_TIMEOUT_MS" => {
                    self.liveness.swim.probe_timeout_ms = parse(k, v)?
                }
                "P2P_LIVENESS_SWIM_INDIRECT_PROBE_TIMEOUT_MS" => {
                    self.liveness.swim.indirect_probe_timeout_ms = parse(k, v)?
                }
                "P2P_BUDGET_MEMORY_BYTES" => self.budget.memory_bytes = parse(k, v)?,
                "P2P_BUDGET_THREADS" => self.budget.threads = parse(k, v)?,
                "P2P_BUDGET_MAX_JOBS" => self.budget.max_jobs = parse(k, v)?,
                "P2P_MIN_TRUST" => self.trust.min_trust = parse(k, v)?,
                "P2P_MIN_ATTESTATION" => self.trust.min_attestation = v.clone(),
                "P2P_CANARY_RATE" => self.trust.canary_rate = parse(k, v)?,
                "P2P_POW_DIFFICULTY_BITS" => self.sybil.pow_difficulty_bits = parse(k, v)?,
                "P2P_WORKER_POOL_SIZE" => self.limits.worker_pool_size = parse(k, v)?,
                "P2P_STORAGE_PROVIDER" => self.storage.provider = v.clone(),
                "P2P_STORAGE_ENDPOINT" => self.storage.endpoint = Some(v.clone()),
                "P2P_STORAGE_REGION" => self.storage.region = Some(v.clone()),
                "P2P_STORAGE_URL_STYLE" => self.storage.url_style = Some(v.clone()),
                "P2P_STORAGE_USE_SSL" => self.storage.use_ssl = Some(parse(k, v)?),
                "P2P_STORAGE_ENABLE_REMOTE_ACCESS" => {
                    self.storage.enable_remote_access = parse(k, v)?
                }
                "P2P_STORAGE_REQUIRE_EXTENSIONS" => self.storage.require_extensions = parse(k, v)?,
                "P2P_STORAGE_PRELOAD_EXTENSIONS" => {
                    self.storage.preload_extensions =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_STORAGE_ENABLED_FORMATS" => {
                    self.storage.enabled_formats =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_STORAGE_ENABLED_PROVIDERS" => {
                    self.storage.enabled_providers =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_STORAGE_ALLOWED_LOCAL_PATHS" => {
                    self.storage.allowed_local_paths =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_PLANNER_ENABLED" => self.planner.enabled = parse(k, v)?,
                "P2P_PLANNER_LOCAL_EXECUTION_ENABLED" | "P2P_PLANNER_LOCAL_EXECUTION" => {
                    self.planner.local_execution_enabled = parse(k, v)?
                }
                "P2P_PLANNER_PREFER" => {
                    self.planner.prefer = match v.trim().to_ascii_lowercase().as_str() {
                        "local" => PreferMode::Local,
                        "remote" => PreferMode::Remote,
                        "auto" => PreferMode::Auto,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown planner prefer {other} (local|remote|auto)"),
                            ))
                        }
                    }
                }
                "P2P_PLANNER_RAM_FRACTION" => self.planner.ram_fraction = parse(k, v)?,
                "P2P_PLANNER_MAX_CONCURRENT_LOCAL_JOBS" => {
                    self.planner.max_concurrent_local_jobs = parse(k, v)?
                }
                "P2P_PLANNER_SIZE_THRESHOLD_BYTES" => {
                    self.planner.size_threshold_bytes = parse(k, v)?
                }
                "P2P_PLANNER_SPILL_TOLERANCE_BYTES" => {
                    self.planner.spill_tolerance_bytes = parse(k, v)?
                }
                "P2P_PLANNER_MAX_LOCAL_LATENCY_MS" => {
                    self.planner.max_local_latency_ms = parse(k, v)?
                }
                // ---- OS sandbox (env layer) ----
                "P2P_SANDBOX_ENABLED" => self.sandbox.enabled = parse(k, v)?,
                "P2P_SANDBOX_BACKEND" => {
                    self.sandbox.backend = match v.trim().to_ascii_lowercase().as_str() {
                        "auto" => SandboxBackend::Auto,
                        "none" => SandboxBackend::None,
                        "rlimit" => SandboxBackend::Rlimit,
                        "cgroups-seccomp" | "cgroups" | "cgroup" | "seccomp" | "linux" => {
                            SandboxBackend::CgroupsSeccomp
                        }
                        "macos-seatbelt" | "macos" | "seatbelt" => SandboxBackend::MacosSeatbelt,
                        "windows-jobobject" | "windows" | "jobobject" | "job-object" => {
                            SandboxBackend::WindowsJobObject
                        }
                        "android" => SandboxBackend::Android,
                        "ios" => SandboxBackend::Ios,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!(
                                    "unknown sandbox backend {other} (auto|none|rlimit|\
                                     cgroups-seccomp|macos-seatbelt|windows-jobobject|android|ios)"
                                ),
                            ))
                        }
                    }
                }
                "P2P_SANDBOX_LIMITS_MODE" => {
                    self.sandbox.limits.mode = match v.trim().to_ascii_lowercase().as_str() {
                        "inherit_budget" | "inherit" => SandboxLimitsMode::InheritBudget,
                        "explicit" => SandboxLimitsMode::Explicit,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown sandbox limits mode {other} (inherit_budget|explicit)"),
                            ))
                        }
                    }
                }
                "P2P_SANDBOX_MEMORY_BYTES" => self.sandbox.limits.memory_bytes = parse(k, v)?,
                "P2P_SANDBOX_CPU_SECONDS" => self.sandbox.limits.cpu_seconds = parse(k, v)?,
                "P2P_SANDBOX_MAX_FILE_SIZE_BYTES" => {
                    self.sandbox.limits.max_file_size_bytes = parse(k, v)?
                }
                "P2P_SANDBOX_MAX_OPEN_FILES" => self.sandbox.limits.max_open_files = parse(k, v)?,
                "P2P_SANDBOX_MAX_PROCESSES" => self.sandbox.limits.max_processes = parse(k, v)?,
                "P2P_SANDBOX_EGRESS_MODE" => {
                    self.sandbox.egress_mode = match v.trim().to_ascii_lowercase().as_str() {
                        "inherit_storage" | "inherit" => SandboxEgressMode::InheritStorage,
                        "explicit" => SandboxEgressMode::Explicit,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown sandbox egress mode {other} (inherit_storage|explicit)"),
                            ))
                        }
                    }
                }
                "P2P_SANDBOX_EGRESS_ALLOWLIST" => {
                    self.sandbox.egress_allowlist =
                        v.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
                }
                "P2P_SANDBOX_TEMP_DIR_POLICY" => {
                    self.sandbox.temp_dir_policy = match v.trim().to_ascii_lowercase().as_str() {
                        "ephemeral" => SandboxTempDirPolicy::Ephemeral,
                        "inherit" => SandboxTempDirPolicy::Inherit,
                        "custom" => SandboxTempDirPolicy::Custom,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown sandbox temp dir policy {other} (ephemeral|inherit|custom)"),
                            ))
                        }
                    }
                }
                "P2P_SANDBOX_TEMP_DIR" => self.sandbox.temp_dir = Some(v.clone()),
                // ---- economics / settlement layer (env layer) ----
                "P2P_ECONOMICS_ENABLED" => self.economics.enabled = parse(k, v)?,
                "P2P_ECONOMICS_DEFAULT_PAYMENT" => {
                    self.economics.default_payment = match v.as_str() {
                        "free" => PaymentPref::Free,
                        "paid" => PaymentPref::Paid,
                        "auto" => PaymentPref::Auto,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown payment mode {other} (free|paid|auto)"),
                            ))
                        }
                    }
                }
                "P2P_ECONOMICS_FEE_RECIPIENT" => self.economics.fee_recipient = Some(v.clone()),
                "P2P_ECONOMICS_NETWORK" => {
                    self.economics.network = match v.as_str() {
                        "testnet" => economics::TonNetwork::Testnet,
                        "mainnet" => economics::TonNetwork::Mainnet,
                        other => {
                            return Err(ConfigError::Env(
                                k.clone(),
                                format!("unknown network {other} (testnet|mainnet)"),
                            ))
                        }
                    }
                }
                "P2P_ECONOMICS_MAINNET_CONFIRMED" => {
                    self.economics.mainnet_confirmed = parse(k, v)?
                }
                // ---- anti-abuse / robustness layer (env layer) ----
                "P2P_ANTIABUSE_ENABLED" => self.antiabuse.enabled = parse(k, v)?,
                "P2P_ANTIABUSE_FAULT_ATTRIBUTION_ENABLED" => {
                    self.antiabuse.fault_attribution.enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_JOB_CONSENSUS_FRACTION" => {
                    self.antiabuse.fault_attribution.job_consensus_fraction = parse(k, v)?
                }
                "P2P_ANTIABUSE_REQUESTER_TRUST_ENABLED" => {
                    self.antiabuse.requester_trust.enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_REQUESTER_NEGATIVE_FLOOR" => {
                    self.antiabuse.requester_trust.negative_floor_weight = parse(k, v)?
                }
                "P2P_ANTIABUSE_REQUESTER_POSITIVE_FLOOR" => {
                    self.antiabuse.requester_trust.positive_floor_weight = parse(k, v)?
                }
                "P2P_ANTIABUSE_REQUESTER_AGE_SATURATION" => {
                    self.antiabuse.requester_trust.age_saturation = parse(k, v)?
                }
                "P2P_ANTIABUSE_COST_GATE_ENABLED" => {
                    self.antiabuse.cost_gate.enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_COST_GATE_MAX_COST_HINT_ROWS" => {
                    self.antiabuse.cost_gate.max_cost_hint_rows = parse(k, v)?
                }
                "P2P_ANTIABUSE_COST_GATE_MAX_WORKING_SET_FACTOR" => {
                    self.antiabuse.cost_gate.max_working_set_factor = parse(k, v)?
                }
                "P2P_ANTIABUSE_NONDETERMINISM_ENABLED" => {
                    self.antiabuse.nondeterminism.enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_FREE_RATE_LIMIT_ENABLED" => {
                    self.antiabuse.free_rate_limit.enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_FREE_RATE_MAX_PER_WINDOW" => {
                    self.antiabuse.free_rate_limit.max_free_per_window = parse(k, v)?
                }
                "P2P_ANTIABUSE_FREE_RATE_WINDOW_SECS" => {
                    self.antiabuse.free_rate_limit.window_secs = parse(k, v)?
                }
                "P2P_ANTIABUSE_FREE_RATE_REQUIRE_POW_BITS" => {
                    self.antiabuse.free_rate_limit.require_pow_bits = parse(k, v)?
                }
                "P2P_ANTIABUSE_AUTO_BLOCK_ENABLED" => {
                    self.antiabuse.blocklist.auto_block_enabled = parse(k, v)?
                }
                "P2P_ANTIABUSE_AUTO_BLOCK_TRUST_FLOOR" => {
                    self.antiabuse.blocklist.auto_block_trust_floor = parse(k, v)?
                }
                "P2P_ANTIABUSE_HONOR_GOSSIP_SIGNALS" => {
                    self.antiabuse.blocklist.honor_gossip_signals = parse(k, v)?
                }
                "P2P_ANTIABUSE_HONOR_GLOBAL_PARAMS" => {
                    self.antiabuse.blocklist.honor_global_params = parse(k, v)?
                }
                "P2P_ANTIABUSE_GOSSIP_PEER_SCORING" => {
                    self.antiabuse.gossip.peer_scoring = parse(k, v)?
                }
                "P2P_ANTIABUSE_GOSSIP_DIVERSE_BOOTSTRAP" => {
                    self.antiabuse.gossip.diverse_bootstrap = parse(k, v)?
                }
                // P2P_CONFIG is handled in `load`, ignore here.
                "P2P_CONFIG" => {}
                _ => {} // ignore unknown P2P_* to stay forward-compatible
            }
        }
        Ok(())
    }

    /// Validate cross-field invariants and ranges.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let inv = |m: String| Err(ConfigError::Invalid(m));

        // Protocol versions must parse and min_supported must not exceed version.
        let version: p2p_proto::Version = self
            .protocol
            .version
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("protocol.version: {e}")))?;
        let min: p2p_proto::Version = self
            .protocol
            .min_supported_version
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("protocol.min_supported_version: {e}")))?;
        if min > version {
            return inv(format!(
                "protocol.min_supported_version ({min}) must be <= protocol.version ({version})"
            ));
        }

        if self.scheduler.replicas == 0 {
            return inv("scheduler.replicas must be >= 1".into());
        }
        if self.scheduler.quorum == 0 {
            return inv("scheduler.quorum must be >= 1".into());
        }
        if self.scheduler.quorum > self.scheduler.replicas {
            return inv(format!(
                "scheduler.quorum ({}) must be <= scheduler.replicas ({})",
                self.scheduler.quorum, self.scheduler.replicas
            ));
        }
        if self.discovery.candidate_sample_size < self.scheduler.replicas {
            return inv(format!(
                "discovery.candidate_sample_size ({}) must be >= scheduler.replicas ({})",
                self.discovery.candidate_sample_size, self.scheduler.replicas
            ));
        }

        // ---- resilience / re-dispatch ----
        let sch = &self.scheduler;
        if !(0.0..=1.0).contains(&sch.backoff_jitter_frac) {
            return inv(format!(
                "scheduler.backoff_jitter_frac must be in [0,1], got {}",
                sch.backoff_jitter_frac
            ));
        }
        if sch.backoff_max_ms < sch.backoff_initial_ms {
            return inv(format!(
                "scheduler.backoff_max_ms ({}) must be >= scheduler.backoff_initial_ms ({})",
                sch.backoff_max_ms, sch.backoff_initial_ms
            ));
        }
        if sch.retry_budget_max_tokens < 0.0 {
            return inv("scheduler.retry_budget_max_tokens must be >= 0".into());
        }
        if sch.retry_budget_refill_per_sec < 0.0 {
            return inv("scheduler.retry_budget_refill_per_sec must be >= 0".into());
        }

        // ---- liveness: phi-accrual + SWIM ----
        let phi = &self.liveness.phi;
        if phi.convict_threshold <= 0.0 {
            return inv("liveness.phi.convict_threshold must be > 0".into());
        }
        if phi.window_size == 0 {
            return inv("liveness.phi.window_size must be >= 1".into());
        }
        if phi.min_std_ms <= 0.0 {
            return inv("liveness.phi.min_std_ms must be > 0".into());
        }
        if phi.first_interval_ms <= 0.0 {
            return inv("liveness.phi.first_interval_ms must be > 0".into());
        }
        if self.discovery.gossip.topic.trim().is_empty() {
            return inv("discovery.gossip.topic must be non-empty".into());
        }
        if self.discovery.gossip.fanout == 0 {
            return inv("discovery.gossip.fanout must be >= 1".into());
        }

        // ---- NAT traversal ----
        let nat = &self.discovery.nat;
        if nat.dcutr && !nat.relay_client {
            return inv(
                "discovery.nat.dcutr requires discovery.nat.relay_client = true (DCUtR hole \
                 punching coordinates over an existing relayed connection)"
                    .into(),
            );
        }
        if nat.relay_client && nat.max_relays == 0 {
            return inv(
                "discovery.nat.max_relays must be >= 1 when discovery.nat.relay_client = true"
                    .into(),
            );
        }
        if nat.external_addresses.iter().any(|a| a.trim().is_empty()) {
            return inv("discovery.nat.external_addresses entries must be non-empty".into());
        }
        if nat.relays.iter().any(|a| a.trim().is_empty()) {
            return inv("discovery.nat.relays entries must be non-empty".into());
        }
        if nat.act_as_relay {
            let rl = &nat.relay_limits;
            if rl.max_reservations == 0 {
                return inv(
                    "discovery.nat.relay_limits.max_reservations must be >= 1 when act_as_relay = true"
                        .into(),
                );
            }
            if rl.max_circuits == 0 {
                return inv(
                    "discovery.nat.relay_limits.max_circuits must be >= 1 when act_as_relay = true"
                        .into(),
                );
            }
        }
        let pct = |name: &str, x: f64| -> Result<(), ConfigError> {
            if !(0.0..=1.0).contains(&x) {
                return Err(ConfigError::Invalid(format!("{name} must be in [0,1], got {x}")));
            }
            Ok(())
        };
        pct("trust.min_trust", self.trust.min_trust)?;
        pct("trust.canary_rate", self.trust.canary_rate)?;
        pct("trust.incorrect_penalty", self.trust.incorrect_penalty)?;
        pct("trust.bootstrap_trust", self.trust.bootstrap_trust)?;
        if !matches!(self.trust.min_attestation.as_str(), "L0" | "L1" | "L2") {
            return inv(format!(
                "trust.min_attestation must be L0|L1|L2, got {}",
                self.trust.min_attestation
            ));
        }
        if self.budget.threads == 0 {
            return inv("budget.threads must be >= 1".into());
        }
        if self.budget.max_jobs == 0 {
            return inv("budget.max_jobs must be >= 1".into());
        }
        if self.network.keepalive_ms == 0 {
            return inv("network.keepalive_ms must be >= 1".into());
        }
        if self.network.idle_timeout_ms == 0 {
            return inv("network.idle_timeout_ms must be >= 1 (0 would disable the idle timeout, letting a stuck connection live forever)".into());
        }
        if self.network.keepalive_ms >= self.network.idle_timeout_ms {
            return inv("network.keepalive_ms must be < network.idle_timeout_ms".into());
        }
        if self.network.connect_timeout_ms == 0 {
            return inv("network.connect_timeout_ms must be >= 1 (0 would let a dial to an unresponsive peer hang forever)".into());
        }
        if self.limits.worker_pool_size == 0 {
            return inv("limits.worker_pool_size must be >= 1".into());
        }

        // ---- identity / peer pinning ----
        if matches!(self.identity.pinning_mode, PinningMode::Allowlist) {
            if self.identity.allowlist.is_empty() {
                return inv(
                    "identity.allowlist must be non-empty when identity.pinning_mode = \"allowlist\" (otherwise the node rejects every peer)".into(),
                );
            }
            for entry in &self.identity.allowlist {
                let is_node_id = entry
                    .strip_prefix("b3:")
                    .map(|h| {
                        h.len() == 64
                            && h.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                    .unwrap_or(false);
                if !is_node_id {
                    return inv(format!(
                        "identity.allowlist entry {entry:?} must be a node id of the form b3:<64 lowercase hex chars>"
                    ));
                }
            }
        }

        // ---- storage / data sources ----
        if self.storage.enabled_formats.is_empty() {
            return inv("storage.enabled_formats must list at least one format".into());
        }
        if self.storage.enabled_providers.is_empty() {
            return inv("storage.enabled_providers must list at least one provider".into());
        }
        if self.storage.enabled_providers.iter().any(|p| p.trim().is_empty()) {
            return inv("storage.enabled_providers entries must be non-empty".into());
        }
        // S3 URL addressing style (top-level + any per-provider override) must be
        // a value DuckDB understands, so a typo fails closed at config time.
        let valid_url_style = |s: &str| matches!(s.trim().to_ascii_lowercase().as_str(), "path" | "vhost");
        if let Some(s) = &self.storage.url_style {
            if !valid_url_style(s) {
                return inv(format!("storage.url_style must be 'path' or 'vhost', got '{s}'"));
            }
        }
        for (id, kv) in &self.storage.provider_options {
            if let Some(s) = kv.get("url_style") {
                if !valid_url_style(s) {
                    return inv(format!(
                        "storage.provider_options.{id}.url_style must be 'path' or 'vhost', got '{s}'"
                    ));
                }
            }
        }

        // ---- planner ----
        let p = &self.planner;
        if !(p.ram_fraction > 0.0 && p.ram_fraction <= 1.0) {
            return inv(format!(
                "planner.ram_fraction must be in (0,1], got {}",
                p.ram_fraction
            ));
        }
        if p.enabled && p.max_concurrent_local_jobs == 0 {
            return inv(
                "planner.max_concurrent_local_jobs must be >= 1 when planner.enabled = true".into(),
            );
        }

        // ---- transport tuning ----
        let q = &self.transport.quic;
        // QUIC flow-control windows are carried as QUIC `VarInt`s (max 2^62 - 1).
        const VARINT_MAX: u64 = (1u64 << 62) - 1;
        let check_window = |name: &str, x: u64| -> Result<(), ConfigError> {
            if x == 0 {
                return Err(ConfigError::Invalid(format!("{name} must be >= 1")));
            }
            if x > VARINT_MAX {
                return Err(ConfigError::Invalid(format!(
                    "{name} ({x}) exceeds the QUIC VarInt maximum ({VARINT_MAX})"
                )));
            }
            Ok(())
        };
        if let Some(w) = q.stream_receive_window_bytes {
            check_window("transport.quic.stream_receive_window_bytes", w)?;
        }
        if let Some(w) = q.connection_receive_window_bytes {
            check_window("transport.quic.connection_receive_window_bytes", w)?;
        }
        check_window("transport.quic.send_window_bytes", q.send_window_bytes)?;
        if q.max_concurrent_uni_streams == 0 {
            return inv("transport.quic.max_concurrent_uni_streams must be >= 1".into());
        }
        if q.enable_0rtt && q.session_ticket_lifetime_secs == 0 {
            return inv(
                "transport.quic.session_ticket_lifetime_secs must be >= 1 when enable_0rtt = true"
                    .into(),
            );
        }
        if q.bdp.enabled {
            if q.bdp.bandwidth_mbps == 0 {
                return inv("transport.quic.bdp.bandwidth_mbps must be >= 1 when enabled".into());
            }
            if q.bdp.rtt_ms == 0 {
                return inv("transport.quic.bdp.rtt_ms must be >= 1 when enabled".into());
            }
            check_window("transport.quic.bdp target", q.bdp.target_bytes())?;
        }
        let r = &self.transport.result;
        if r.parallelism == 0 {
            return inv("transport.result.parallelism must be >= 1".into());
        }
        if r.parallelism > q.max_concurrent_uni_streams as usize {
            return inv(format!(
                "transport.result.parallelism ({}) must be <= transport.quic.max_concurrent_uni_streams ({})",
                r.parallelism, q.max_concurrent_uni_streams
            ));
        }
        if let Some(c) = r.chunk_bytes {
            if c == 0 {
                return inv("transport.result.chunk_bytes must be >= 1".into());
            }
        }
        if r.max_result_bytes == 0 {
            return inv("transport.result.max_result_bytes must be >= 1".into());
        }
        if r.max_result_parts == 0 {
            return inv("transport.result.max_result_parts must be >= 1".into());
        }
        let c = &self.transport.compression;
        if matches!(c.algorithm, CompressionAlgo::Zstd) && !(1..=22).contains(&c.level) {
            return inv(format!(
                "transport.compression.level ({}) must be in 1..=22 for zstd",
                c.level
            ));
        }

        // ---- OS sandbox ----
        let sb = &self.sandbox;
        if matches!(sb.temp_dir_policy, SandboxTempDirPolicy::Custom)
            && sb.temp_dir.as_deref().map(str::trim).unwrap_or("").is_empty()
        {
            return inv(
                "sandbox.temp_dir must be set when sandbox.temp_dir_policy = \"custom\"".into(),
            );
        }
        if matches!(sb.egress_mode, SandboxEgressMode::Explicit)
            && sb.egress_allowlist.iter().any(|e| e.trim().is_empty())
        {
            return inv("sandbox.egress_allowlist entries must be non-empty".into());
        }

        // ---- economics / settlement layer ----
        self.economics.validate()?;

        // ---- anti-abuse / robustness layer ----
        self.antiabuse.validate()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        GridConfig::default().validate().unwrap();
    }

    #[test]
    fn toml_layer_overrides_defaults_only_where_set() {
        let toml = r#"
            [scheduler]
            replicas = 5
            quorum = 3
        "#;
        let cfg = GridConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.scheduler.replicas, 5);
        assert_eq!(cfg.scheduler.quorum, 3);
        // untouched field keeps default
        assert_eq!(cfg.scheduler.offer_timeout_ms, 2_000);
        // untouched section keeps default
        assert_eq!(cfg.budget.threads, 2);
        cfg.validate().unwrap();
    }

    #[test]
    fn env_layer_overrides_file_layer() {
        let mut cfg = GridConfig::from_toml_str("[scheduler]\nreplicas = 5\nquorum = 3\n").unwrap();
        let mut env = BTreeMap::new();
        env.insert("P2P_QUORUM".to_string(), "2".to_string());
        env.insert("P2P_BIND_ADDR".to_string(), "0.0.0.0:9494".to_string());
        env.insert(
            "P2P_BOOTSTRAP".to_string(),
            "quic://a:9494,quic://b:9494".to_string(),
        );
        cfg.apply_env_map(&env).unwrap();
        assert_eq!(cfg.scheduler.quorum, 2);
        assert_eq!(cfg.scheduler.replicas, 5); // unchanged by env
        assert_eq!(cfg.network.bind_addr, "0.0.0.0:9494");
        assert_eq!(cfg.discovery.bootstrap.len(), 2);
        cfg.validate().unwrap();
    }

    #[test]
    fn economics_env_layer_overrides_file_layer() {
        // defaults -> TOML -> env layering for the [economics] section.
        let mut cfg =
            GridConfig::from_toml_str("[economics]\nenabled = false\ndefault_payment = \"auto\"\n")
                .unwrap();
        let mut env = BTreeMap::new();
        env.insert("P2P_ECONOMICS_ENABLED".to_string(), "true".to_string());
        env.insert("P2P_ECONOMICS_DEFAULT_PAYMENT".to_string(), "paid".to_string());
        env.insert("P2P_ECONOMICS_FEE_RECIPIENT".to_string(), "EQ_treasury".to_string());
        cfg.apply_env_map(&env).unwrap();
        assert!(cfg.economics.enabled);
        assert_eq!(cfg.economics.default_payment, PaymentPref::Paid);
        assert_eq!(cfg.economics.fee_recipient.as_deref(), Some("EQ_treasury"));
        // settlement is still noop by default, so validation passes.
        cfg.validate().unwrap();
    }

    #[test]
    fn nat_defaults_are_on_and_valid() {
        let cfg = GridConfig::default();
        let nat = &cfg.discovery.nat;
        assert!(nat.autonat);
        assert!(nat.dcutr);
        assert!(nat.relay_client);
        assert!(!nat.act_as_relay);
        assert!(nat.mdns);
        assert_eq!(nat.max_relays, 3);
        assert!(nat.relays.is_empty());
        assert_eq!(nat.relay_limits.max_reservations, 128);
        cfg.validate().unwrap();
    }

    #[test]
    fn nat_layers_defaults_then_toml_then_env() {
        let toml = r#"
            [discovery.nat]
            act_as_relay = true
            mdns = false
            max_relays = 5
            relays = ["/ip4/203.0.113.10/udp/9595/quic-v1/p2p/12D3KooWtest"]

            [discovery.nat.relay_limits]
            max_circuits = 32
        "#;
        let mut cfg = GridConfig::from_toml_str(toml).unwrap();
        assert!(cfg.discovery.nat.act_as_relay);
        assert!(!cfg.discovery.nat.mdns);
        assert_eq!(cfg.discovery.nat.max_relays, 5);
        assert_eq!(cfg.discovery.nat.relays.len(), 1);
        assert_eq!(cfg.discovery.nat.relay_limits.max_circuits, 32);
        // untouched relay-limit field keeps its default
        assert_eq!(cfg.discovery.nat.relay_limits.max_reservations, 128);

        // ENV overrides the TOML layer.
        let mut env = BTreeMap::new();
        env.insert("P2P_DISCOVERY_NAT_AUTONAT".to_string(), "false".to_string());
        env.insert("P2P_DISCOVERY_NAT_MAX_RELAYS".to_string(), "2".to_string());
        env.insert(
            "P2P_DISCOVERY_NAT_RELAYS".to_string(),
            "/ip4/198.51.100.7/udp/9595/quic-v1/p2p/12D3KooWa,/ip4/198.51.100.8/udp/9595/quic-v1/p2p/12D3KooWb".to_string(),
        );
        cfg.apply_env_map(&env).unwrap();
        assert!(!cfg.discovery.nat.autonat);
        assert_eq!(cfg.discovery.nat.max_relays, 2);
        assert_eq!(cfg.discovery.nat.relays.len(), 2);
        // dcutr (untouched) stays default-on; act_as_relay stays from TOML.
        assert!(cfg.discovery.nat.dcutr);
        assert!(cfg.discovery.nat.act_as_relay);
        cfg.validate().unwrap();
    }

    #[test]
    fn nat_validation_rejects_dcutr_without_relay_client() {
        let cfg =
            GridConfig::from_toml_str("[discovery.nat]\ndcutr = true\nrelay_client = false\n")
                .unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err}").contains("dcutr"), "got {err}");
    }

    #[test]
    fn validation_rejects_quorum_gt_replicas() {
        let cfg = GridConfig::from_toml_str("[scheduler]\nreplicas = 2\nquorum = 3\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validation_rejects_out_of_range_trust() {
        let cfg = GridConfig::from_toml_str("[trust]\nmin_trust = 1.5\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields guards against silent typos in operator config.
        assert!(GridConfig::from_toml_str("[scheduler]\nreplicaz = 5\n").is_err());
    }

    #[test]
    fn roundtrips_through_toml() {
        let cfg = GridConfig::default();
        let text = cfg.to_toml();
        let back = GridConfig::from_toml_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn transport_defaults_and_bdp_sizing() {
        let cfg = GridConfig::default();
        assert!(cfg.transport.quic.gso);
        assert_eq!(cfg.transport.quic.congestion, CongestionAlgo::Cubic);
        assert_eq!(cfg.transport.result.parallelism, 1);
        assert_eq!(cfg.transport.compression.algorithm, CompressionAlgo::None);
        // Non-BDP: inherit network windows + explicit send window.
        let (s, c, snd) = cfg.transport.quic.effective_windows(&cfg.network);
        assert_eq!(s, cfg.network.stream_receive_window);
        assert_eq!(c, cfg.network.receive_window);
        assert_eq!(snd, cfg.transport.quic.send_window_bytes);
        // BDP target: 1000 Mbit/s * 50 ms = 6.25 MB.
        let bdp = BdpConfig { enabled: true, bandwidth_mbps: 1000, rtt_ms: 50 };
        assert_eq!(bdp.target_bytes(), 6_250_000);
    }

    #[test]
    fn transport_validation_rejects_parallelism_over_uni_cap() {
        let toml = "[transport.quic]\nmax_concurrent_uni_streams = 4\n[transport.result]\nparallelism = 8\n";
        let cfg = GridConfig::from_toml_str(toml).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn transport_env_overrides_congestion_and_parallelism() {
        let mut cfg = GridConfig::default();
        let mut env = BTreeMap::new();
        env.insert("P2P_TRANSPORT_CONGESTION".to_string(), "bbr".to_string());
        env.insert("P2P_TRANSPORT_RESULT_PARALLELISM".to_string(), "4".to_string());
        env.insert("P2P_TRANSPORT_COMPRESSION".to_string(), "zstd".to_string());
        cfg.apply_env_map(&env).unwrap();
        assert_eq!(cfg.transport.quic.congestion, CongestionAlgo::Bbr);
        assert_eq!(cfg.transport.result.parallelism, 4);
        assert_eq!(cfg.transport.compression.algorithm, CompressionAlgo::Zstd);
        cfg.validate().unwrap();
    }

    #[test]
    fn planner_defaults_are_valid_and_auto() {
        let cfg = GridConfig::default();
        assert!(cfg.planner.enabled);
        // Local execution is enabled by default (local-first); remote-only is opt-in.
        assert!(cfg.planner.local_execution_enabled);
        assert_eq!(cfg.planner.prefer, PreferMode::Auto);
        assert_eq!(cfg.planner.ram_fraction, 0.6);
        assert_eq!(cfg.planner.max_concurrent_local_jobs, 4);
        cfg.validate().unwrap();
    }

    #[test]
    fn remote_only_mode_layers_defaults_then_file_then_env() {
        // Default: local execution enabled.
        assert!(GridConfig::default().planner.local_execution_enabled);

        // FILE layer disables local execution (remote-only mode) + sticky remote.
        let toml = r#"
            [planner]
            local_execution_enabled = false
            prefer = "remote"
        "#;
        let mut cfg = GridConfig::from_toml_str(toml).unwrap();
        assert!(!cfg.planner.local_execution_enabled);
        assert_eq!(cfg.planner.prefer, PreferMode::Remote);

        // ENV layer overrides the file layer (re-enables local execution).
        let mut env = BTreeMap::new();
        env.insert(
            "P2P_PLANNER_LOCAL_EXECUTION_ENABLED".to_string(),
            "true".to_string(),
        );
        cfg.apply_env_map(&env).unwrap();
        assert!(cfg.planner.local_execution_enabled);
        // env did not touch prefer set by the file layer
        assert_eq!(cfg.planner.prefer, PreferMode::Remote);
        cfg.validate().unwrap();
    }

    #[test]
    fn planner_layers_defaults_then_toml_then_env() {
        // TOML layer overrides a subset of defaults.
        let toml = r#"
            [planner]
            prefer = "local"
            ram_fraction = 0.25
            size_threshold_bytes = 1048576
        "#;
        let mut cfg = GridConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.planner.prefer, PreferMode::Local);
        assert_eq!(cfg.planner.ram_fraction, 0.25);
        assert_eq!(cfg.planner.size_threshold_bytes, 1_048_576);
        // untouched planner field keeps its default
        assert_eq!(cfg.planner.max_concurrent_local_jobs, 4);

        // ENV layer overrides the TOML layer.
        let mut env = BTreeMap::new();
        env.insert("P2P_PLANNER_PREFER".to_string(), "remote".to_string());
        env.insert(
            "P2P_PLANNER_MAX_CONCURRENT_LOCAL_JOBS".to_string(),
            "2".to_string(),
        );
        cfg.apply_env_map(&env).unwrap();
        assert_eq!(cfg.planner.prefer, PreferMode::Remote);
        assert_eq!(cfg.planner.max_concurrent_local_jobs, 2);
        // env did not touch ram_fraction set by TOML
        assert_eq!(cfg.planner.ram_fraction, 0.25);
        cfg.validate().unwrap();
    }

    #[test]
    fn planner_validation_rejects_bad_ram_fraction() {
        let cfg = GridConfig::from_toml_str("[planner]\nram_fraction = 0.0\n").unwrap();
        assert!(cfg.validate().is_err());
        let cfg = GridConfig::from_toml_str("[planner]\nram_fraction = 1.5\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn sandbox_defaults_are_off_and_valid() {
        let cfg = GridConfig::default();
        assert!(!cfg.sandbox.enabled);
        assert_eq!(cfg.sandbox.backend, SandboxBackend::Auto);
        assert_eq!(cfg.sandbox.egress_mode, SandboxEgressMode::InheritStorage);
        assert_eq!(cfg.sandbox.limits.mode, SandboxLimitsMode::InheritBudget);
        assert_eq!(cfg.sandbox.temp_dir_policy, SandboxTempDirPolicy::Ephemeral);
        cfg.validate().unwrap();
    }

    #[test]
    fn sandbox_layers_defaults_then_toml_then_env() {
        let toml = r#"
            [sandbox]
            enabled = true
            backend = "rlimit"
            egress_mode = "explicit"
            egress_allowlist = ["s3.amazonaws.com:443"]

            [sandbox.limits]
            mode = "explicit"
            max_file_size_bytes = 1048576
        "#;
        let mut cfg = GridConfig::from_toml_str(toml).unwrap();
        assert!(cfg.sandbox.enabled);
        assert_eq!(cfg.sandbox.backend, SandboxBackend::Rlimit);
        assert_eq!(cfg.sandbox.egress_mode, SandboxEgressMode::Explicit);
        assert_eq!(cfg.sandbox.limits.mode, SandboxLimitsMode::Explicit);
        assert_eq!(cfg.sandbox.limits.max_file_size_bytes, 1_048_576);
        // env overrides the TOML layer
        let mut env = BTreeMap::new();
        env.insert("P2P_SANDBOX_BACKEND".to_string(), "windows-jobobject".to_string());
        env.insert("P2P_SANDBOX_MAX_OPEN_FILES".to_string(), "64".to_string());
        env.insert("P2P_SANDBOX_EGRESS_MODE".to_string(), "inherit_storage".to_string());
        cfg.apply_env_map(&env).unwrap();
        assert_eq!(cfg.sandbox.backend, SandboxBackend::WindowsJobObject);
        assert_eq!(cfg.sandbox.limits.max_open_files, 64);
        assert_eq!(cfg.sandbox.egress_mode, SandboxEgressMode::InheritStorage);
        // untouched TOML field survives env layer
        assert_eq!(cfg.sandbox.limits.max_file_size_bytes, 1_048_576);
        cfg.validate().unwrap();
    }

    #[test]
    fn sandbox_backend_names_roundtrip_through_toml() {
        for (name, want) in [
            ("auto", SandboxBackend::Auto),
            ("none", SandboxBackend::None),
            ("rlimit", SandboxBackend::Rlimit),
            ("cgroups-seccomp", SandboxBackend::CgroupsSeccomp),
            ("macos-seatbelt", SandboxBackend::MacosSeatbelt),
            ("windows-jobobject", SandboxBackend::WindowsJobObject),
            ("android", SandboxBackend::Android),
            ("ios", SandboxBackend::Ios),
        ] {
            let toml = format!("[sandbox]\nbackend = \"{name}\"\n");
            let cfg = GridConfig::from_toml_str(&toml).unwrap();
            assert_eq!(cfg.sandbox.backend, want, "parsing {name}");
            // And it serializes back to the same kebab name.
            assert!(cfg.to_toml().contains(&format!("backend = \"{name}\"")));
        }
    }

    #[test]
    fn sandbox_validation_requires_temp_dir_for_custom_policy() {
        let cfg = GridConfig::from_toml_str(
            "[sandbox]\nenabled = true\ntemp_dir_policy = \"custom\"\n",
        )
        .unwrap();
        assert!(cfg.validate().is_err());
        let cfg = GridConfig::from_toml_str(
            "[sandbox]\ntemp_dir_policy = \"custom\"\ntemp_dir = \"/var/tmp/p2p\"\n",
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn bad_env_value_reports_var_name() {
        let mut cfg = GridConfig::default();
        let mut env = BTreeMap::new();
        env.insert("P2P_QUORUM".to_string(), "notanumber".to_string());
        let err = cfg.apply_env_map(&env).unwrap_err();
        assert!(format!("{err}").contains("P2P_QUORUM"));
    }
}
