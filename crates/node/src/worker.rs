//! Worker (host) service: answers Offers with Bids, executes Dispatches under a
//! budget lease, commits a result hash first, then streams the result only if it
//! wins (architecture §3, §10, §11).

use std::sync::Arc;
use std::time::{Duration, Instant};

use p2p_proto::{
    Ack, Attestation, Bid, BidDecision, Compression, Dispatch, Offer, Progress, ResultCommit, Wire,
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

/// Result of running a job under the progress/heartbeat + deadline wrapper.
enum ExecOutcome {
    /// Execution completed with a result.
    Ok(ResultSet),
    /// The host execution deadline was exceeded — the job is abandoned.
    Abandoned,
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
}

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

    /// Attach an OS-level execution sandbox (architecture §9.4) resolved from
    /// config. The egress allow-list and read-only fixture paths are derived
    /// from the `[storage]` section so a job can reach object storage but
    /// nothing else. When `cfg.sandbox.enabled = false` this is a no-op and the
    /// worker behaves exactly as before.
    pub fn with_sandbox(mut self, cfg: &p2p_config::GridConfig) -> Self {
        let sandbox = sandbox::build(&cfg.sandbox);
        // LOUD warning: with the no-op sandbox backend there is NO OS-level
        // process isolation or egress control — `enter_job` is a documented
        // no-op, so untrusted queries run in-process. If remote object-storage
        // access is ALSO enabled, the only thing standing between a malicious
        // query and arbitrary network/filesystem egress is the DuckDB
        // configuration lockdown (which cannot scope network egress). Make sure
        // the operator does not believe the deployment is OS-sandboxed.
        if sandbox.name() == "noop" && cfg.storage.enable_remote_access {
            warn!(
                sandbox_backend = sandbox.name(),
                "NO OS-LEVEL SANDBOX: storage.enable_remote_access=true but the effective \
                 sandbox backend is 'noop' (no process isolation, no egress filtering). \
                 Untrusted queries run in-process and DuckDB cannot restrict network egress \
                 to specific endpoints. Run hosts behind an OS-level egress firewall, or \
                 enable a real sandbox backend ([sandbox]), before exposing remote reads."
            );
        }
        // Opt-in process-per-job OS enforcement (G1/G8) is requested but the
        // enforced child-execution path (a sandboxed DuckDB subprocess + its
        // result/credential transfer protocol) is not yet wired: be loud that jobs
        // still run IN-PROCESS so an operator never believes a job that exceeds its
        // lease will be OS-killed. The in-process default is fully intact.
        if cfg.sandbox.process_per_job {
            warn!(
                sandbox_backend = sandbox.name(),
                "[sandbox].process_per_job is enabled, but enforced process-per-job execution \
                 is NOT yet wired — jobs continue to run in-process under the DuckDB lockdown. \
                 See ARCHITECTURE.md §9.4 for the remaining subprocess protocol."
            );
        }
        self.sandbox = SandboxPolicy {
            sandbox,
            cfg: Arc::new(cfg.sandbox.clone()),
            egress: Arc::new(EgressAllowList::derive(&cfg.sandbox, &cfg.storage)),
            read_only_paths: Arc::new(cfg.storage.allowed_local_paths.clone()),
            remote_access: cfg.storage.enable_remote_access,
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
                let bid = self.make_bid(&offer);
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
        }
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
    /// + admission-control decision). This is exactly what the worker replies to
    /// an inbound `Offer`; exposed for tests and tooling.
    pub fn bid_for(&self, offer: &Offer) -> Bid {
        self.make_bid(offer)
    }

    /// Admission-control decision for an offer.
    fn make_bid(&self, offer: &Offer) -> Bid {
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

        Bid {
            job_id: offer.job_id.clone(),
            worker_id,
            decision,
            eta_ms,
            price: 0,
            attestation: self.attestation.clone(),
            recent_receipts: vec![],
            free_mem_bytes: free.memory_bytes,
            free_threads: free.threads,
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

        // Build the per-job context: the scoped, short-lived storage credential
        // (turned into a prefix-scoped DuckDB secret by the real engine). The
        // mock engine ignores it.
        let ctx = crate::engine::JobContext {
            credential: dispatch.credential.clone(),
            parquet_keys: Vec::new(),
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
        let exec = self
            .execute_with_progress(
                &dispatch, mem, threads, &ctx, &mut send, &mut recv, &worker_id, start,
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
