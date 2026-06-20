//! Worker (host) service: answers Offers with Bids, executes Dispatches under a
//! budget lease, commits a result hash first, then streams the result only if it
//! wins (architecture §3, §10, §11).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use p2p_proto::{
    Ack, Attestation, Bid, BidDecision, Compression, Dispatch, InputSnapshot, Offer, Progress,
    ResultCommit, Wire,
};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, QuicTransport, RecvStream, SendStream, Transport};
use p2p_trust::canonical_hash;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::admission::AdmissionController;
use crate::antiabuse::{cost_gate_reason, Blocklist, RateLimiter};
use crate::engine::EngineError;
use crate::liveness::now_ms;
use p2p_proto::ResultSet;

/// Decode a hex ed25519 pubkey into 32 bytes (`None` on malformed input).
fn decode_pubkey(hex_s: &str) -> Option<[u8; 32]> {
    hex::decode(hex_s).ok()?.try_into().ok()
}

/// What a worker actually read for a job's pinned inputs (deterministic-input
/// verification). Returned by an [`InputReader`] before execution.
#[derive(Debug, Clone)]
pub enum InputObservation {
    /// No pin to honor (the dispatch carried no snapshot) — report an empty
    /// fingerprint. The requester treats empty as "on the pinned snapshot".
    Unpinned,
    /// The pinned snapshot was read; report this fingerprint. When it equals the
    /// dispatched fingerprint the pin was honored; when it differs the worker
    /// read a different (e.g. newer) version — the requester calls that benign
    /// drift, never a fault.
    Pinned(String),
    /// The pinned object version could not be fetched/validated (changed, gone,
    /// or the store was unreachable). A job/input fault → no provider penalty.
    Unavailable(String),
}

/// Verifies/reports which input snapshot a worker read for a job. The default
/// [`EchoInputReader`] trusts the pinned dispatch (echoes its fingerprint) —
/// correct for the in-process engine path and for tests where the data layer is
/// mocked. A real object-store-backed reader re-HEADs each pinned object and
/// reports the OBSERVED fingerprint (architecture P3 re-validation) or signals
/// the pin is unfetchable.
#[async_trait]
pub trait InputReader: Send + Sync {
    async fn observe(&self, snapshot: Option<&InputSnapshot>) -> InputObservation;
}

/// Default reader: echo the dispatched pin's fingerprint (no re-validation).
pub struct EchoInputReader;

#[async_trait]
impl InputReader for EchoInputReader {
    async fn observe(&self, snapshot: Option<&InputSnapshot>) -> InputObservation {
        match snapshot {
            Some(s) => InputObservation::Pinned(s.fingerprint.clone()),
            None => InputObservation::Unpinned,
        }
    }
}

/// Result of running a job under the progress/heartbeat + deadline wrapper.
enum ExecOutcome {
    /// Execution completed with a result.
    Ok(ResultSet),
    /// The host execution deadline was exceeded — the job is abandoned.
    Abandoned,
    /// The metered **cap deadline** (`Dispatch::cap_deadline_ms`, derived from the
    /// bid's `cap_seconds`) was exceeded — hard-abort exactly at the cap and report
    /// an explicit resource/job fault to the requester (no provider penalty/slash).
    CapExceeded,
    /// The requester cancelled this job mid-execution (it lost the hedged race,
    /// or the dispatch stream was reset). We stop computing immediately instead
    /// of finishing the now-useless query, freeing the budget for other jobs.
    Cancelled,
    /// Execution errored (forwarded as an `Ack { ok: false }` to the requester).
    Err(EngineError),
}
use crate::compression::algo_to_wire;
use crate::engine::{ExecLease, QueryEngine};
use crate::result_stream::SendOpts;
use crate::sandbox::{self, EgressAllowList, JobBudget, ResourceLimits, Sandbox, SandboxSpec};
use p2p_config::SandboxConfig;

/// Configuration values the worker needs (subset of `GridConfig`), passed in so
/// nothing is hard-coded.
#[derive(Debug, Clone)]
pub struct WorkerParams {
    pub per_job_memory_bytes: u64,
    pub per_job_threads: u32,
    /// Bulk result chunk size (bytes) for backpressured streaming.
    pub result_chunk_bytes: usize,
    /// Default number of concurrent result-transfer streams (a dispatch may
    /// override this per call).
    pub result_parallelism: usize,
    /// Only fan a result out across streams when it is at least this large.
    pub parallel_min_bytes: usize,
    /// Default wire compression codec (a dispatch may override this per call).
    pub compression: Compression,
    /// Compression level (zstd) and minimum payload size to compress.
    pub compression_level: i32,
    pub compression_min_bytes: usize,
    /// Hard cap on concurrent uni-streams (from the QUIC tuning); the effective
    /// parallelism is clamped to this so we never exceed the transport limit.
    pub max_uni_streams: usize,
    /// Host execution deadline: a job running longer than this is **abandoned**
    /// (architecture §11). `Duration::ZERO` = no host-side deadline.
    pub job_timeout: Duration,
    /// How often the host streams a progress/heartbeat update to the requester
    /// while a job executes. `Duration::ZERO` disables progress streaming.
    pub progress_interval: Duration,
    // --- Request-scoping / routing labels (from `[membership]` + `[worker]`). ---
    /// Whether this host accepts NEW offers. `false` = graceful standby/drain
    /// (declines new offers in `make_bid`; in-flight leases finish).
    pub enabled: bool,
    /// Logical grid partitions this host serves (empty ⇒ the `"default"` one).
    pub networks: Vec<String>,
    /// Group memberships this host serves (empty ⇒ ungrouped / public).
    pub groups: Vec<String>,
    /// Declared region of this host (`None` ⇒ unspecified).
    pub region: Option<String>,
    /// Group-membership enforcement tier (`soft` trusts declared claims; `token`
    /// verifies the requester's `group_proof` against `group_issuers`).
    pub group_enforcement: p2p_config::GroupEnforcement,
    /// Trusted group issuer pubkeys (group → hex ed25519 key) for the token tier.
    pub group_issuers: std::collections::BTreeMap<String, String>,
    /// This host's region-attestation proof (JSON `CapabilityToken`), attached to
    /// accepting bids so attested-tier requesters can verify residency.
    pub region_token: Option<String>,
    /// Pricing policy this host advertises in its bids (`[economics.pricing]`).
    /// Under the metered model with a non-zero rate the bid carries per-second/
    /// per-GiB rates + an `estimated_seconds`/`cap_seconds`; otherwise the metered
    /// fields stay `0` (fixed-price / free — today's behavior).
    pub pricing: p2p_config::PricingEconomics,
    /// Closed enterprise posture (`[security].mode = "private"`). When set, the
    /// host serves ONLY rostered, grouped requesters: an ungrouped host refuses
    /// every offer (`require_grouped_hosts`) and a requester not on the roster is
    /// declined (default-deny), on top of the always-on peer-id binding. `false`
    /// (default / public) ⇒ today's open behavior.
    pub private: bool,
    /// Default-deny requester roster for private mode (reuses the identity
    /// allowlist node ids). Empty under the public default. Only consulted when
    /// `private` is set.
    pub roster: Vec<String>,
}

/// Conservative cold-start throughput (bytes/second) a host assumes for its
/// metered `estimated_seconds` bid when it has no measured self-capability yet.
/// Deliberately pessimistic (16 MiB/s) so a fresh node bids a GENEROUS cap rather
/// than over-promising speed it cannot prove — over-promising only loses it the
/// race (a perf-score hit), never a slash.
const COLD_START_THROUGHPUT_BPS: u64 = 16 * 1024 * 1024;

impl WorkerParams {
    /// Derive worker params from a full config.
    pub fn from_config(cfg: &p2p_config::GridConfig) -> Self {
        Self {
            per_job_memory_bytes: cfg.budget.per_job_memory_bytes,
            per_job_threads: cfg.budget.per_job_threads,
            result_chunk_bytes: cfg
                .transport
                .result
                .chunk_bytes
                .unwrap_or(cfg.network.result_chunk_bytes),
            result_parallelism: cfg.transport.result.parallelism,
            parallel_min_bytes: cfg.transport.result.parallel_min_bytes,
            compression: algo_to_wire(cfg.transport.compression.algorithm),
            compression_level: cfg.transport.compression.level,
            compression_min_bytes: cfg.transport.compression.min_size_bytes,
            max_uni_streams: cfg.transport.quic.max_concurrent_uni_streams as usize,
            job_timeout: Duration::from_millis(cfg.worker.job_timeout_ms),
            progress_interval: Duration::from_millis(cfg.worker.progress_interval_ms),
            enabled: cfg.worker.enabled,
            networks: cfg.membership.networks.clone(),
            groups: cfg.membership.groups.clone(),
            region: cfg.membership.region.clone(),
            group_enforcement: cfg.membership.group_enforcement,
            group_issuers: cfg.membership.group_issuers.clone(),
            region_token: cfg.membership.region_token.clone(),
            pricing: cfg.economics.pricing.clone(),
            private: cfg.is_private(),
            // The roster reuses the identity allowlist (the set of node ids this
            // node trusts at the transport layer) so private mode has a single
            // source of truth for "who is a member".
            roster: cfg.identity.allowlist.clone(),
        }
    }

    /// Build the [`SendOpts`] for one dispatch, applying per-call overrides from
    /// the requester and clamping the fan-out to the transport's uni-stream cap.
    fn send_opts(&self, dispatch: &Dispatch) -> SendOpts {
        let parallelism = dispatch
            .result_parallelism
            .map(|p| p as usize)
            .unwrap_or(self.result_parallelism)
            .clamp(1, self.max_uni_streams.max(1));
        let compression = dispatch.compression.unwrap_or(self.compression);
        SendOpts {
            chunk_bytes: self.result_chunk_bytes,
            parallelism,
            parallel_min_bytes: self.parallel_min_bytes,
            compression,
            compression_level: self.compression_level,
            compression_min_bytes: self.compression_min_bytes,
        }
    }
}

/// The OS-level execution sandbox the worker wraps around job execution
/// (architecture §9.4). Resolved once from config; the per-job [`SandboxSpec`]
/// is built from the lease + this policy at dispatch time.
#[derive(Clone)]
struct SandboxPolicy {
    sandbox: Arc<dyn Sandbox>,
    cfg: Arc<SandboxConfig>,
    /// Network egress allow-list derived from the `[storage]` config.
    egress: Arc<EgressAllowList>,
    /// Read-only fixture dirs the job may access.
    read_only_paths: Arc<Vec<String>>,
    /// Whether storage remote access is enabled (gates network egress at all).
    remote_access: bool,
}

impl SandboxPolicy {
    fn disabled() -> Self {
        Self {
            sandbox: Arc::new(sandbox::NoopSandbox),
            cfg: Arc::new(SandboxConfig::default()),
            egress: Arc::new(EgressAllowList::default()),
            read_only_paths: Arc::new(Vec::new()),
            remote_access: false,
        }
    }
}

/// Worker-side anti-abuse runtime (ARCHITECTURE "Abuse resistance"): refuse
/// blocked requesters, decline over-budget offers up front, and rate-limit FREE
/// jobs per requester identity. Off (all `None`) unless wired via
/// [`Worker::with_antiabuse`], so the default worker behaves exactly as before.
#[derive(Clone)]
struct AntiAbuseRuntime {
    cfg: Arc<p2p_config::AntiAbuseConfig>,
    economics: Arc<p2p_config::EconomicsConfig>,
    blocklist: Option<Arc<Blocklist>>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

/// A running worker service.
#[derive(Clone)]
pub struct Worker {
    transport: Arc<QuicTransport>,
    engine: Arc<dyn QueryEngine>,
    admission: Arc<AdmissionController>,
    attestation: Attestation,
    params: WorkerParams,
    sandbox: SandboxPolicy,
    antiabuse: Option<AntiAbuseRuntime>,
    /// Optional durable self-capability profile store: when set, each successful
    /// execution's MEASURED magnitude is folded into the node's signed
    /// `CapabilityProfile`. `None` (default) ⇒ no profile is written.
    capability: Option<Arc<crate::capability_store::CapabilityStore>>,
    /// Optional real attestor (architecture §7.2/§9.3): when wired, each bid's
    /// attestation is produced PER-OFFER, binding `attest_bound_pub` to this
    /// node's environment and freshened by the offer nonce — the honest L1/L2
    /// honor-path a verifier-equipped requester checks. `None` (default, all
    /// shipped hosts) ⇒ the fixed L0 stub, unchanged.
    attestor: Option<Arc<dyn p2p_trust::attestation::Attestor>>,
    attest_bound_pub: [u8; 32],
    /// Reports which input snapshot the worker actually read (deterministic-input
    /// verification). Defaults to [`EchoInputReader`] (trusts the dispatched pin).
    input_reader: Arc<dyn InputReader>,
    /// Fail-safe serving block (architecture §9.4, G8): when `Some(reason)` the
    /// host REFUSES every offer with that reason (a clean decline — no receipt,
    /// no score effect). Set when a host would otherwise serve foreign jobs in an
    /// unsafe posture (e.g. `storage.enable_remote_access=true` without an active
    /// OS egress filter): we keep running under the DuckDB lockdown but never
    /// serve a remote-access job unconfined, rather than crashing or silently
    /// allowing open egress. `None` (default) ⇒ serve normally.
    serving_block: Option<Arc<str>>,
}

impl Worker {
    pub fn new(
        transport: Arc<QuicTransport>,
        engine: Arc<dyn QueryEngine>,
        admission: Arc<AdmissionController>,
        attestation: Attestation,
        params: WorkerParams,
    ) -> Self {
        Self {
            transport,
            engine,
            admission,
            attestation,
            params,
            sandbox: SandboxPolicy::disabled(),
            antiabuse: None,
            capability: None,
            attestor: None,
            attest_bound_pub: [0u8; 32],
            input_reader: Arc::new(EchoInputReader),
            serving_block: None,
        }
    }

    /// Put this host into the fail-safe serving block (architecture §9.4, G8):
    /// it keeps running under the DuckDB lockdown but DECLINES every offer with
    /// `reason`. Used when serving foreign jobs would be unsafe (e.g. remote
    /// object-store access enabled without an active OS egress filter). A clean
    /// decline — no receipt, no score effect — never a crash or open egress.
    pub fn refusing_to_serve(mut self, reason: impl Into<String>) -> Self {
        self.serving_block = Some(Arc::from(reason.into()));
        self
    }

    /// Wire a custom [`InputReader`] (deterministic-input verification). A real
    /// object-store-backed reader re-HEADs the pinned objects and reports the
    /// OBSERVED fingerprint / signals an unfetchable pin; the default echoes the
    /// dispatched pin.
    pub fn with_input_reader(mut self, reader: Arc<dyn InputReader>) -> Self {
        self.input_reader = reader;
        self
    }

    /// Wire a real (or software) attestor that produces per-offer, nonce-bound
    /// attestation evidence binding `bound_pub` (e.g. the node's sealing key) to
    /// its environment. The matching requester runs an `AttestationVerifier`.
    pub fn with_attestor(
        mut self,
        attestor: Arc<dyn p2p_trust::attestation::Attestor>,
        bound_pub: [u8; 32],
    ) -> Self {
        self.attestor = Some(attestor);
        self.attest_bound_pub = bound_pub;
        self
    }

    /// The attestation to present for `offer`: a per-offer nonce-bound quote when
    /// an attestor is wired (the honor-path), else the fixed stub.
    fn attestation_for(&self, offer: &Offer) -> Attestation {
        match &self.attestor {
            Some(a) => a.produce(&offer.nonce.to_le_bytes(), &self.attest_bound_pub),
            None => self.attestation.clone(),
        }
    }

    /// Wire a durable self-capability profile store (architecture §3 routing):
    /// each successful execution's measured rows/bytes are folded into the node's
    /// signed, monotonic [`crate::capability_store::CapabilityProfile`]. Off by
    /// default; enabling it is the host opt-in (`[worker].self_measure_capability`).
    pub fn with_capability_store(
        mut self,
        store: Arc<crate::capability_store::CapabilityStore>,
    ) -> Self {
        self.capability = Some(store);
        self
    }

    /// Wire the anti-abuse runtime (ARCHITECTURE "Abuse resistance"): a shared
    /// deny-list and a free-job rate limiter, both consulted in `make_bid`. The
    /// behaviors are gated by `cfg.antiabuse` (defaults preserve today's
    /// behavior). A `None` blocklist/limiter disables that specific check.
    pub fn with_antiabuse(
        mut self,
        cfg: &p2p_config::GridConfig,
        blocklist: Option<Arc<Blocklist>>,
        rate_limiter: Option<Arc<RateLimiter>>,
    ) -> Self {
        self.antiabuse = Some(AntiAbuseRuntime {
            cfg: Arc::new(cfg.antiabuse.clone()),
            economics: Arc::new(cfg.economics.clone()),
            blocklist,
            rate_limiter,
        });
        self
    }

    /// Attach an OS-level execution sandbox (architecture §9.4) resolved from the
    /// HOST-serving sandbox config + the `[storage]` section. The egress
    /// allow-list and read-only fixture paths are derived from `[storage]` so a
    /// job can reach object storage but nothing else. When `sandbox_cfg.enabled =
    /// false` this is a no-op and the worker behaves exactly as before.
    ///
    /// Takes the sandbox config explicitly (not the whole `GridConfig`) so the
    /// caller can pass the **host-serving** posture — the secure defaults applied
    /// at `Node::spawn_host` — without forcing those defaults onto the requester's
    /// own self-run path.
    pub fn with_sandbox(
        mut self,
        sandbox_cfg: &SandboxConfig,
        storage_cfg: &p2p_config::StorageConfig,
    ) -> Self {
        let sandbox = sandbox::build(sandbox_cfg);
        // LOUD warning: with the no-op sandbox backend there is NO OS-level
        // process isolation or egress control — `enter_job` is a documented
        // no-op, so untrusted queries run in-process. If remote object-storage
        // access is ALSO enabled, the only thing standing between a malicious
        // query and arbitrary network/filesystem egress is the DuckDB
        // configuration lockdown (which cannot scope network egress). Make sure
        // the operator does not believe the deployment is OS-sandboxed.
        if sandbox.name() == "noop" && storage_cfg.enable_remote_access {
            warn!(
                sandbox_backend = sandbox.name(),
                "NO OS-LEVEL SANDBOX: storage.enable_remote_access=true but the effective \
                 sandbox backend is 'noop' (no process isolation, no egress filtering). \
                 Untrusted queries run in-process and DuckDB cannot restrict network egress \
                 to specific endpoints. Run hosts behind an OS-level egress firewall, or \
                 enable a real sandbox backend ([sandbox]), before exposing remote reads."
            );
        }
        // Opt-in process-per-job OS enforcement (G1/G8) is wired end-to-end in
        // `Node::host_engine` (it swaps the in-process engine for the OS-sandboxed
        // `SubprocessEngine` when `process_per_job` is on AND a `P2P_JOB_EXEC`
        // child executor is configured). The flag-on-but-unconfigured case warns +
        // falls back to in-process THERE, so no (now-stale) warning is emitted here.
        self.sandbox = SandboxPolicy {
            sandbox,
            cfg: Arc::new(sandbox_cfg.clone()),
            egress: Arc::new(EgressAllowList::derive(sandbox_cfg, storage_cfg)),
            read_only_paths: Arc::new(storage_cfg.allowed_local_paths.clone()),
            remote_access: storage_cfg.enable_remote_access,
        };
        self
    }

    /// Spawn the accept loop; returns a handle. Drop/abort to stop, or close the
    /// transport.
    pub fn spawn(self) -> JoinHandle<()> {
        tokio::spawn(async move { self.serve().await })
    }

    /// Accept inbound connections until the endpoint closes.
    pub async fn serve(self) {
        while let Some(incoming) = self.transport.accept().await {
            match incoming {
                Ok(conn) => {
                    let worker = self.clone();
                    tokio::spawn(async move { worker.handle_connection(conn).await });
                }
                Err(e) => {
                    debug!("accept failed: {e}");
                }
            }
        }
    }

    async fn handle_connection(self, conn: Conn) {
        loop {
            match conn.accept_bi().await {
                Ok((send, recv)) => {
                    let worker = self.clone();
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        if let Err(e) = worker.handle_stream(conn, send, recv).await {
                            debug!("stream handler ended: {e}");
                        }
                    });
                }
                Err(_) => break, // connection closed
            }
        }
    }

    async fn handle_stream(
        self,
        conn: Conn,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> p2p_transport::Result<()> {
        let msg = read_msg(&mut recv).await?;
        match msg {
            Wire::Offer(offer) => {
                // Bind the application-level `requester_id` to the authenticated
                // mTLS peer (P0): the honest requester dials us directly and sends
                // its OWN offer, so they always match; a mismatch is impersonation.
                let bid = self.make_bid_authenticated(conn.peer_node_id(), &offer);
                write_msg(&mut send, &Wire::Bid(bid)).await?;
                let _ = send.finish();
            }
            Wire::Dispatch(dispatch) => {
                self.handle_dispatch(conn, dispatch, send, recv).await?;
            }
            other => {
                warn!("unexpected first message on stream: {other:?}");
            }
        }
        Ok(())
    }

    /// Build a rejecting bid with a given reason (no admission, no receipt).
    fn reject_bid(&self, offer: &Offer, reason: impl Into<String>) -> Bid {
        let free = self.admission.free();
        Bid {
            job_id: offer.job_id.clone(),
            worker_id: self.transport.local_node_id().clone(),
            decision: BidDecision::Reject {
                reason: reason.into(),
            },
            eta_ms: 0,
            price: 0,
            attestation: self.attestation.clone(),
            recent_receipts: vec![],
            free_mem_bytes: free.memory_bytes,
            free_threads: free.threads,
            region_proof: None,
            rate_per_second: 0,
            rate_per_gb: 0,
            estimated_seconds: 0,
            cap_seconds: 0,
        }
    }

    /// Compute this host's metered bid terms for an offer (time-based pricing):
    /// the advertised per-second/per-GiB rates plus an `estimated_seconds` from the
    /// data-size hint ÷ the host's throughput and `cap_seconds = estimated ×
    /// cap_multiplier`. All-zero unless the metered model is active with a rate, so
    /// a fixed/free host bids no metered terms (today's behavior).
    fn metered_bid_terms(&self, offer: &Offer) -> (u64, u64, u64, u64) {
        let p = &self.params.pricing;
        if !p.metered_active() {
            return (0, 0, 0, 0);
        }
        let rate_per_second = p.effective_rate_per_second().min(u64::MAX as u128) as u64;
        // Estimate the scanned bytes from the offer's byte hint (the estimator's
        // scan estimate), falling back to a rows→bytes heuristic, then to 0.
        const AVG_ROW_BYTES: u64 = 256;
        let bytes = offer
            .cost_hint_bytes
            .or_else(|| offer.cost_hint_rows.map(|r| r.saturating_mul(AVG_ROW_BYTES)))
            .unwrap_or(0);
        // Cold-start: a conservative measured-throughput assumption (a real host
        // would refine this from its self-capability profile). ceil(bytes/tput),
        // floored at 1 second so even a tiny job has a non-zero estimate/cap.
        let throughput = COLD_START_THROUGHPUT_BPS.max(1);
        let estimated_seconds = bytes.div_ceil(throughput).max(1);
        let cap_seconds = estimated_seconds
            .saturating_mul(p.cap_multiplier.max(1))
            .max(1);
        (rate_per_second, p.rate_per_gb, estimated_seconds, cap_seconds)
    }

    /// Pre-admission anti-abuse gates (ARCHITECTURE "Abuse resistance"): refuse a
    /// blocked requester, decline an over-budget offer, and rate-limit FREE jobs
    /// per requester identity. Returns a rejecting `Bid` to short-circuit, or
    /// `None` to continue to normal admission. A rejection is **not** an
    /// execution failure — it produces no receipt and never affects a score.
    fn antiabuse_reject(&self, offer: &Offer) -> Option<Bid> {
        let rt = self.antiabuse.as_ref()?;
        if !rt.cfg.enabled {
            return None;
        }
        // 1. Deny-list: refuse a blocked requester (node id).
        if let Some(bl) = &rt.blocklist {
            if bl.is_blocked(offer.requester_id.as_str()) {
                return Some(self.reject_bid(offer, "requester is blocklisted"));
            }
        }
        // 2. Pre-flight cost gate: decline an over-budget query up front.
        if rt.cfg.cost_gate_active() {
            if let Some(reason) = cost_gate_reason(
                &rt.cfg.cost_gate,
                offer.cost_hint_rows,
                None,
                self.params.per_job_memory_bytes,
            ) {
                return Some(self.reject_bid(offer, reason));
            }
        }
        // 3. Free-mode rate limit (paid jobs are prioritized and bypass it).
        if rt.cfg.free_rate_limit_active() {
            let class = match offer.data_class {
                p2p_proto::DataClass::Public => p2p_config::DataClassCfg::Public,
                p2p_proto::DataClass::Internal => p2p_config::DataClassCfg::Internal,
                p2p_proto::DataClass::Sensitive => p2p_config::DataClassCfg::Sensitive,
            };
            let is_free = rt.economics.resolve_payment(class).is_free();
            let prioritize_paid = rt.cfg.free_rate_limit.prioritize_paid;
            if is_free || !prioritize_paid {
                if let Some(rl) = &rt.rate_limiter {
                    if !rl.check_and_record(&offer.requester_id, p2p_trust::now_ts()) {
                        return Some(self.reject_bid(offer, "free-job rate limit exceeded"));
                    }
                }
            }
        }
        None
    }

    /// Evaluate an offer and produce this worker's [`Bid`] (the anti-abuse gates
    /// + admission-control decision), WITHOUT the transport-identity binding.
    /// Exposed for tests and tooling that have no live connection; the live wire
    /// path uses [`Worker::bid_for_peer`] / `make_bid_authenticated` so the
    /// offer's self-claimed `requester_id` is bound to the authenticated peer.
    pub fn bid_for(&self, offer: &Offer) -> Bid {
        self.make_bid(offer)
    }

    /// Evaluate an offer received from an AUTHENTICATED mTLS `peer`: enforce the
    /// application↔transport identity binding (P0), then normal admission. As
    /// [`Worker::bid_for`] but with the peer-id binding the live path applies.
    pub fn bid_for_peer(&self, peer: &p2p_proto::NodeId, offer: &Offer) -> Bid {
        self.make_bid_authenticated(peer, offer)
    }

    /// Wire-path bid: reject any offer whose self-claimed `requester_id` does not
    /// equal the connection's authenticated `peer_node_id` (P0 — impersonation
    /// defense), then defer to normal admission. ALWAYS-ON (public and private):
    /// it is a pure correctness/security invariant. The honest flow always
    /// matches — the requester dials the host itself and stamps its own node id
    /// into the offer (`coordinator.rs`: `requester_id = local_node_id`), and the
    /// QUIC data plane is point-to-point (no relayed/proxied offer path; libp2p
    /// relays live only on the separate discovery overlay) — so a mismatch can
    /// only be a peer presenting an offer under another node's identity. Binding
    /// `requester_id == peer` is also what makes a STOLEN `group_proof` useless:
    /// the token is bound to its holder's id (`token_proves_group`), so a thief
    /// on a different TLS identity is rejected here before the group check.
    fn make_bid_authenticated(&self, peer: &p2p_proto::NodeId, offer: &Offer) -> Bid {
        if &offer.requester_id != peer {
            return self.reject_bid(
                offer,
                "offer requester_id does not match the authenticated mTLS peer",
            );
        }
        self.make_bid(offer)
    }

    /// Request-scoping admission gate (architecture §7.5): reject an offer this
    /// host should not serve — it is on standby (graceful drain), the offer targets
    /// a logical network this host isn't in, the host is grouped and the requester
    /// shares none of its groups, or the offer pins regions and this host isn't in
    /// one. All checks are no-ops when unset (zero-config host serves everything).
    /// A rejection is an honest decline (no receipt, no score effect).
    fn membership_reject(&self, offer: &Offer) -> Option<Bid> {
        let p = &self.params;
        // 1. Standby / graceful drain: decline NEW offers; in-flight leases finish.
        if !p.enabled {
            return Some(self.reject_bid(offer, "host is on standby (draining)"));
        }
        // 1a. Private/enterprise closure (`[security].mode = "private"`). Two
        //     extra fail-closed gates on top of the always-on peer-id binding and
        //     the token group check below; both no-ops in public mode.
        if p.private {
            // require_grouped_hosts: an UNGROUPED host serves everyone (the group
            // check below is skipped when it has no groups) — exactly what a
            // closed grid must not do. Refuse outright until the host declares the
            // company group(s) it serves.
            if p.groups.is_empty() {
                return Some(self.reject_bid(
                    offer,
                    "private mode: host declares no membership.groups (an ungrouped host \
                     must not serve in a closed grid)",
                ));
            }
            // Default-deny roster: serve only requesters on the roster (the
            // identity allowlist), not merely those not on a reactive blocklist.
            // On the wire path `requester_id == peer` (bound above) and the peer
            // already cleared the TLS allowlist, so this is defense-in-depth and
            // makes the membership policy explicit.
            if !p.roster.iter().any(|r| r == offer.requester_id.as_str()) {
                return Some(self.reject_bid(
                    offer,
                    "private mode: requester is not on the host roster (identity.allowlist)",
                ));
            }
        }
        // 2. Network partition: the offer targets a partition this host doesn't
        //    serve. An unlabeled host is in the implicit "default" partition.
        if let Some(net) = &offer.network {
            let in_net = if p.networks.is_empty() {
                net == "default"
            } else {
                p.networks.iter().any(|n| n == net)
            };
            if !in_net {
                return Some(self.reject_bid(offer, "host is not in the requested network"));
            }
        }
        // 3. Group membership: a grouped host serves a requester only if it can
        //    prove a shared group. The SOFT tier (default) trusts the requester's
        //    declared `offer.groups`; the TOKEN tier verifies a cryptographic
        //    `group_proof` against the configured issuer for one of the host's
        //    groups (bound to the requester's identity). An ungrouped (public)
        //    host ignores this entirely.
        if !p.groups.is_empty() {
            let shares = match p.group_enforcement {
                p2p_config::GroupEnforcement::Soft => {
                    offer.groups.iter().any(|g| p.groups.contains(g))
                }
                p2p_config::GroupEnforcement::Token => self.token_proves_group(offer),
            };
            if !shares {
                return Some(
                    self.reject_bid(offer, "requester shares none of the host's groups"),
                );
            }
        }
        // 4. Region pin: only hosts in a requested region qualify; a host with no
        //    declared region fails closed (a region-pinned query needs certainty).
        if !offer.regions.is_empty() {
            let in_region = match &p.region {
                Some(r) => offer.regions.iter().any(|x| x == r),
                None => false,
            };
            if !in_region {
                return Some(self.reject_bid(offer, "host region not in the requested set"));
            }
        }
        None
    }

    /// Token-tier group check: the requester's `group_proof` must verify against
    /// the issuer configured for one of this host's groups, AND the proven holder
    /// key must hash to the requester's node id (so a proof can't be replayed by a
    /// different node). Any parse/verify/binding failure ⇒ not proven (fail-closed).
    fn token_proves_group(&self, offer: &Offer) -> bool {
        let Some(proof) = &offer.group_proof else {
            return false;
        };
        let Ok(token) = serde_json::from_str::<p2p_trust::CapabilityToken>(proof) else {
            return false;
        };
        let now = p2p_trust::now_ts();
        for hg in &self.params.groups {
            let Some(issuer_hex) = self.params.group_issuers.get(hg) else {
                continue;
            };
            let Some(issuer) = decode_pubkey(issuer_hex) else {
                continue;
            };
            if let Ok(holder) = p2p_trust::verify_group_membership(&token, &issuer, hg, now) {
                if p2p_proto::NodeId::from_pubkey(&holder) == offer.requester_id {
                    return true;
                }
            }
        }
        false
    }

    /// Admission-control decision for an offer.
    fn make_bid(&self, offer: &Offer) -> Bid {
        // Fail-safe serving block (§9.4, G8): refuse EVERY offer up front when the
        // host is in an unsafe-to-serve posture (e.g. remote access without an OS
        // egress filter). A clean decline — no receipt, no score effect.
        if let Some(reason) = &self.serving_block {
            return self.reject_bid(offer, reason.to_string());
        }
        if let Some(rejection) = self.membership_reject(offer) {
            return rejection;
        }
        if let Some(rejection) = self.antiabuse_reject(offer) {
            return rejection;
        }
        let worker_id = self.transport.local_node_id().clone();
        let free = self.admission.free();

        if !self.admission.serves_data_class(offer.data_class) {
            return Bid {
                job_id: offer.job_id.clone(),
                worker_id,
                decision: BidDecision::Reject {
                    reason: "data class not served".into(),
                },
                eta_ms: 0,
                price: 0,
                attestation: self.attestation.clone(),
                recent_receipts: vec![],
                free_mem_bytes: free.memory_bytes,
                free_threads: free.threads,
                region_proof: None,
                rate_per_second: 0,
                rate_per_gb: 0,
                estimated_seconds: 0,
                cap_seconds: 0,
            };
        }

        let need_mem = self.params.per_job_memory_bytes;
        let admit = free.free_jobs > 0
            && free.memory_bytes >= need_mem
            && free.threads >= self.params.per_job_threads;

        let decision = if admit {
            BidDecision::Accept
        } else {
            BidDecision::Reject {
                reason: "at capacity".into(),
            }
        };

        // Simple ETA model: base latency + a cost term from the hint.
        let eta_ms = 10 + offer.cost_hint_rows.unwrap_or(0) / 1000;
        // Time-based (usage) pricing terms (zeros under the fixed/free model).
        let (rate_per_second, rate_per_gb, estimated_seconds, cap_seconds) =
            self.metered_bid_terms(offer);

        Bid {
            job_id: offer.job_id.clone(),
            worker_id,
            decision,
            eta_ms,
            price: 0,
            // Per-offer nonce-bound attestation when an attestor is wired (honor
            // path); otherwise the fixed L0 stub. The requester's verifier checks
            // the evidence is bound to THIS offer's nonce.
            attestation: self.attestation_for(offer),
            recent_receipts: vec![],
            free_mem_bytes: free.memory_bytes,
            free_threads: free.threads,
            // Attach the host's region-attestation proof so an attested-tier
            // requester can verify residency. Inert under the declared default.
            region_proof: self.params.region_token.clone(),
            rate_per_second,
            rate_per_gb,
            estimated_seconds,
            cap_seconds,
        }
    }

    /// Fold a successful execution's MEASURED magnitude (result rows + serialized
    /// bytes) into the durable self-capability profile. A no-op unless a store is
    /// wired ([`Worker::with_capability_store`]); the node signs the update with
    /// its own identity and a failed write is logged and ignored.
    fn record_capability(&self, result: &ResultSet) {
        let Some(store) = &self.capability else {
            return;
        };
        let m = crate::capability_store::MeasuredExecution {
            input_bytes: 0,
            result_rows: result.row_count() as u64,
            result_bytes: p2p_proto::to_bytes(result)
                .map(|b| b.len() as u64)
                .unwrap_or(0),
            peak_memory_bytes: 0,
            temp_dir_bytes: 0,
        };
        let signer = crate::signer::IdentitySigner(self.transport.identity());
        if let Err(e) = store.observe(&signer, m) {
            debug!("capability profile update failed: {e}");
        }
    }

    /// Execute the job while streaming periodic [`Progress`] heartbeats on the
    /// dispatch stream and enforcing the host execution deadline.
    ///
    /// The progress ticker and the (optional) `job_timeout` race the execution
    /// future. Each tick writes a `Progress` frame (the requester's liveness
    /// signal); the deadline aborts with [`ExecOutcome::Abandoned`]. The first
    /// tick is delayed by one interval, so a fast job commits with no progress
    /// frames (back-compatible with the existing commit-first flow).
    #[allow(clippy::too_many_arguments)]
    async fn execute_with_progress(
        &self,
        dispatch: &Dispatch,
        mem: u64,
        threads: u32,
        ctx: &crate::engine::JobContext,
        send: &mut SendStream,
        recv: &mut RecvStream,
        worker_id: &p2p_proto::NodeId,
        start: Instant,
        cap_deadline: Option<Duration>,
    ) -> ExecOutcome {
        let exec = self.engine.execute_job(
            &dispatch.sql,
            ExecLease {
                memory_bytes: mem,
                threads,
            },
            ctx,
        );
        tokio::pin!(exec);

        // A loser is cancelled by the coordinator BEFORE it would finish: it
        // resets the dispatch stream (or sends a `Cancel`). Racing a read of the
        // dispatch stream against the engine lets a mid-execution loser abort
        // promptly and release its budget, even when `progress_interval == 0`
        // (no ticks would otherwise observe the teardown). Pre-commit the
        // requester sends nothing on this stream, so any readable byte / reset /
        // EOF here is a cancellation signal — never a false positive.
        let cancel_watch = read_msg(recv);
        tokio::pin!(cancel_watch);

        let interval = self.params.progress_interval;
        let deadline = self.params.job_timeout;
        let mut seq: u32 = 0;

        // A never-firing branch when progress streaming / deadline is disabled.
        let mut ticker = if interval.is_zero() {
            None
        } else {
            let mut t = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            Some(t)
        };

        loop {
            // Build the deadline future fresh each iteration (cheap).
            let timeout = async {
                if deadline.is_zero() {
                    std::future::pending::<()>().await
                } else {
                    let remaining = deadline.saturating_sub(start.elapsed());
                    tokio::time::sleep(remaining).await
                }
            };

            // Hard metered cap deadline (no grace window): aborts exactly at the
            // bid's `cap_seconds`, distinct from the host's own `job_timeout`.
            let cap_timeout = async {
                match cap_deadline {
                    Some(cap) => {
                        let remaining = cap.saturating_sub(start.elapsed());
                        tokio::time::sleep(remaining).await
                    }
                    None => std::future::pending::<()>().await,
                }
            };

            tokio::select! {
                biased;
                res = &mut exec => {
                    return match res {
                        Ok(rs) => ExecOutcome::Ok(rs),
                        Err(e) => ExecOutcome::Err(e),
                    };
                }
                // Coordinator cancelled / reset the dispatch stream while we were
                // still computing: abandon immediately (loser of the hedged race).
                _ = &mut cancel_watch => {
                    debug!(job = %dispatch.job_id, "dispatch cancelled mid-execution; aborting job");
                    return ExecOutcome::Cancelled;
                }
                _ = cap_timeout, if cap_deadline.is_some() => {
                    return ExecOutcome::CapExceeded;
                }
                _ = timeout, if !deadline.is_zero() => {
                    return ExecOutcome::Abandoned;
                }
                _ = async { ticker.as_mut().unwrap().tick().await }, if ticker.is_some() => {
                    seq = seq.saturating_add(1);
                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    // Best-effort pct from elapsed vs. the host deadline (coarse;
                    // a real engine would report rows/stages). Capped below 100
                    // so completion is signaled by the Commit, not progress.
                    let pct = if deadline.is_zero() {
                        0
                    } else {
                        ((elapsed_ms.saturating_mul(100)
                            / deadline.as_millis().max(1) as u64)
                            .min(99)) as u8
                    };
                    let progress = Progress {
                        job_id: dispatch.job_id.clone(),
                        worker_id: worker_id.clone(),
                        stage: "executing".to_string(),
                        rows_processed: 0,
                        pct,
                        seq,
                        ts_ms: now_ms(),
                    };
                    // A failed progress write means the requester went away
                    // (cancelled / reset) — stop streaming and let exec finish or
                    // the connection tear down.
                    if write_msg(send, &Wire::Progress(progress)).await.is_err() {
                        // Keep executing; the result will be discarded if the
                        // stream is gone. Disable further ticks.
                        ticker = None;
                    }
                }
            }
        }
    }

    async fn handle_dispatch(
        self,
        conn: Conn,
        dispatch: Dispatch,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> p2p_transport::Result<()> {
        let worker_id = self.transport.local_node_id().clone();
        let mem = if dispatch.memory_limit_bytes == 0 {
            self.params.per_job_memory_bytes
        } else {
            dispatch.memory_limit_bytes
        };
        let threads = if dispatch.threads == 0 {
            self.params.per_job_threads
        } else {
            dispatch.threads
        };

        // Reserve budget at execution time (held until this scope ends).
        let _lease = match self.admission.try_admit(mem, threads) {
            Some(l) => l,
            None => {
                write_msg(
                    &mut send,
                    &Wire::Ack(Ack {
                        job_id: dispatch.job_id,
                        ok: false,
                        detail: "at capacity".into(),
                    }),
                )
                .await?;
                let _ = send.finish();
                return Ok(());
            }
        };

        // Deterministic-input verification: observe which input snapshot we will
        // read BEFORE spending compute. The default reader trusts the dispatched
        // pin; a real reader re-HEADs the pinned objects. If the pinned version
        // cannot be fetched/validated, fail this attempt as an INPUT fault (the
        // detail classifies as `Infeasible` — no provider penalty), not a wrong
        // result.
        let input_fingerprint = match self
            .input_reader
            .observe(dispatch.input_snapshot.as_ref())
            .await
        {
            InputObservation::Unpinned => String::new(),
            InputObservation::Pinned(fp) => fp,
            InputObservation::Unavailable(reason) => {
                write_msg(
                    &mut send,
                    &Wire::Ack(Ack {
                        job_id: dispatch.job_id,
                        ok: false,
                        // "not found" classifies as Verdict::Infeasible (job/input
                        // fault), so an unfetchable pin never penalizes this host.
                        detail: format!("pinned input version not found: {reason}"),
                    }),
                )
                .await?;
                let _ = send.finish();
                return Ok(());
            }
        };

        // Build the per-job context: the scoped, short-lived storage credential
        // (turned into a prefix-scoped DuckDB secret by the real engine) plus the
        // pinned input snapshot (so the real engine reads the pinned VERSIONS).
        // The mock engine ignores both.
        let ctx = crate::engine::JobContext {
            credential: dispatch.credential.clone(),
            parquet_keys: Vec::new(),
            input_snapshot: dispatch.input_snapshot.clone(),
            // Presigned credential mode: per-object signed HTTPS URLs the engine
            // rewrites the SQL to read with no secret on the host.
            signed_inputs: dispatch.signed_inputs.clone(),
        };

        // Engage the OS-level execution sandbox (architecture §9.4) for the
        // lifetime of this job: resolve the per-job spec (resource caps matching
        // the lease + the storage-derived egress allow-list + read-only fixture
        // scope) and enter it. For the shared-process engine path this is a
        // no-op guard (the hard, kill-the-job-not-the-node enforcement is the
        // process-per-job `Sandbox::command` path); when disabled it is a pure
        // no-op so behavior is unchanged.
        // Per-JOB writable scope (not a shared per-process dir): scope the OS
        // sandbox's writable path to this job only. The real DuckDB engine creates
        // its own private 0700 temp dir under here per execution and removes it at
        // job end (see `duckdb_engine::run_locked`).
        let temp_dir = std::env::temp_dir()
            .join(format!(
                "duckdb-p2p-{}-{}",
                std::process::id(),
                dispatch.job_id
            ))
            .display()
            .to_string();
        let sandbox_spec = SandboxSpec {
            limits: ResourceLimits::resolve(
                &self.sandbox.cfg,
                JobBudget {
                    memory_bytes: mem,
                    threads,
                },
            ),
            egress: (*self.sandbox.egress).clone(),
            read_only_paths: (*self.sandbox.read_only_paths).clone(),
            writable_paths: vec![temp_dir],
            allow_network: self.sandbox.remote_access && !self.sandbox.egress.is_empty(),
        };
        let _sandbox_guard = match self.sandbox.sandbox.enter_job(&sandbox_spec) {
            Ok(g) => {
                debug!(backend = self.sandbox.sandbox.name(), "job sandbox engaged");
                g
            }
            Err(e) => {
                warn!("sandbox enter_job failed ({e}); proceeding under DuckDB lockdown only");
                crate::sandbox::JobGuard::noop()
            }
        };

        let start = Instant::now();
        // Run the job while streaming periodic progress/heartbeat updates back to
        // the requester (the progress update IS the liveness signal, §11). The
        // host ABANDONS the job if it exceeds `job_timeout` (the requester then
        // re-dispatches): we simply stop and drop the stream — no commit.
        let cap_deadline = dispatch
            .cap_deadline_ms
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
        let exec = self
            .execute_with_progress(
                &dispatch, mem, threads, &ctx, &mut send, &mut recv, &worker_id, start,
                cap_deadline,
            )
            .await;

        let result = match exec {
            ExecOutcome::Ok(rs) => rs,
            ExecOutcome::Abandoned => {
                // Over the host deadline: abandon. Drop the stream so the
                // requester observes a stall/no-commit and re-dispatches.
                debug!(job = %dispatch.job_id, "host job_timeout exceeded; abandoning job");
                return Ok(());
            }
            ExecOutcome::CapExceeded => {
                // Hard metered cap-deadline overrun: report an EXPLICIT failure so
                // the requester classifies it as a resource/job fault (no provider
                // penalty, no slash) rather than silent abandonment. The node just
                // over-promised its `cap_seconds` and loses the race.
                let cap_ms = dispatch.cap_deadline_ms.unwrap_or(0);
                debug!(job = %dispatch.job_id, cap_ms, "metered cap deadline exceeded; aborting job");
                write_msg(
                    &mut send,
                    &Wire::Ack(Ack {
                        job_id: dispatch.job_id,
                        ok: false,
                        detail: format!(
                            "cap deadline exceeded after {cap_ms}ms: job too large for the agreed bid cap"
                        ),
                    }),
                )
                .await?;
                let _ = send.finish();
                return Ok(());
            }
            ExecOutcome::Cancelled => {
                // Lost the hedged race while still executing: the coordinator
                // already cancelled/reset this stream. Stop now and let the lease
                // release on scope exit — no commit, no wasted further compute.
                return Ok(());
            }
            ExecOutcome::Err(e) => {
                write_msg(
                    &mut send,
                    &Wire::Ack(Ack {
                        job_id: dispatch.job_id,
                        ok: false,
                        detail: format!("exec error: {e}"),
                    }),
                )
                .await?;
                let _ = send.finish();
                return Ok(());
            }
        };

        let result_hash = canonical_hash(&result);
        let latency_ms = start.elapsed().as_millis() as u64;
        let row_count = result.row_count() as u64;

        // Commit-first: send the hash before any data.
        write_msg(
            &mut send,
            &Wire::Commit(ResultCommit {
                job_id: dispatch.job_id.clone(),
                worker_id,
                result_hash,
                row_count,
                latency_ms,
                // Echo the fingerprint of the input snapshot we actually read, so
                // the requester can tell drift (data changed) from a fault.
                input_fingerprint,
            }),
        )
        .await?;

        // Fold this successful execution's measured magnitude into the node's
        // durable self-capability profile (no-op unless wired). Done after the
        // commit so it never delays delivery.
        self.record_capability(&result);

        // Await the requester's decision: proceed (winner) or cancel (loser).
        match read_msg(&mut recv).await {
            Ok(Wire::Ack(a)) if a.ok => {
                // Winner: stream the full result. The encoding (compression +
                // single vs. parallel streams) is chosen from config/per-call
                // overrides and announced in the manifest.
                let opts = self.params.send_opts(&dispatch);
                crate::result_stream::send_result(
                    &conn,
                    &mut send,
                    &dispatch.job_id,
                    &result,
                    &opts,
                )
                .await?;
            }
            _ => {
                // Loser or cancellation: abandon the stream; the lease releases
                // on scope exit, returning the budget immediately.
                let _ = send.finish();
            }
        }
        Ok(())
    }
}
