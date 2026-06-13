//! Worker (host) service: answers Offers with Bids, executes Dispatches under a
//! budget lease, commits a result hash first, then streams the result only if it
//! wins (architecture §3, §10, §11).

use std::sync::Arc;
use std::time::Instant;

use p2p_proto::{
    Ack, Attestation, Bid, BidDecision, Compression, Dispatch, Offer, ResultCommit, Wire,
};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, QuicTransport, RecvStream, SendStream, Transport};
use p2p_trust::canonical_hash;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::admission::AdmissionController;
use crate::compression::algo_to_wire;
use crate::engine::{ExecLease, QueryEngine};
use crate::result_stream::SendOpts;
use crate::sandbox::{
    self, EgressAllowList, JobBudget, ResourceLimits, Sandbox, SandboxSpec,
};
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

/// A running worker service.
#[derive(Clone)]
pub struct Worker {
    transport: Arc<QuicTransport>,
    engine: Arc<dyn QueryEngine>,
    admission: Arc<AdmissionController>,
    attestation: Attestation,
    params: WorkerParams,
    sandbox: SandboxPolicy,
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
        }
    }

    /// Attach an OS-level execution sandbox (architecture §9.4) resolved from
    /// config. The egress allow-list and read-only fixture paths are derived
    /// from the `[storage]` section so a job can reach object storage but
    /// nothing else. When `cfg.sandbox.enabled = false` this is a no-op and the
    /// worker behaves exactly as before.
    pub fn with_sandbox(mut self, cfg: &p2p_config::GridConfig) -> Self {
        let sandbox = sandbox::build(&cfg.sandbox);
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

    /// Admission-control decision for an offer.
    fn make_bid(&self, offer: &Offer) -> Bid {
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
        let admit = free.free_jobs > 0 && free.memory_bytes >= need_mem && free.threads >= self.params.per_job_threads;

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
        let temp_dir = std::env::temp_dir()
            .join(format!("duckdb-p2p-{}", std::process::id()))
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
                debug!(
                    backend = self.sandbox.sandbox.name(),
                    "job sandbox engaged"
                );
                g
            }
            Err(e) => {
                warn!("sandbox enter_job failed ({e}); proceeding under DuckDB lockdown only");
                crate::sandbox::JobGuard::noop()
            }
        };

        let start = Instant::now();
        let exec = self
            .engine
            .execute_job(
                &dispatch.sql,
                ExecLease {
                    memory_bytes: mem,
                    threads,
                },
                &ctx,
            )
            .await;

        let result = match exec {
            Ok(rs) => rs,
            Err(e) => {
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
