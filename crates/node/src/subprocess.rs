//! Opt-in **process-per-job** execution (architecture §9.4, G1/G8).
//!
//! When `[sandbox].process_per_job` is enabled, the host runs each job in a
//! dedicated, OS-sandboxed CHILD process instead of the in-process engine, so a
//! job that exceeds its lease (RAM / CPU / FDs) is killed by the OS without
//! taking the node down. This module is the wire protocol + the [`QueryEngine`]
//! adapter that makes that transparent to the worker:
//!
//! * [`SubprocessEngine`] implements [`QueryEngine`] by spawning the sandboxed
//!   child via [`Sandbox::command`], piping a [`JobRequest`] to its stdin and
//!   reading a [`JobResponse`] from its stdout. The child is killed on drop, so a
//!   loser/cancelled/timed-out job is torn down by the OS.
//! * [`serve_job`] is the child side: it runs the real engine on one request and
//!   writes the response. The deployable `p2p-job-exec` binary wires it to the
//!   locked-down `DuckDbEngine`.
//!
//! The in-process default is fully intact: this engine is constructed ONLY when
//! the flag is on AND an executor binary is configured (`P2P_JOB_EXEC`); the
//! worker code itself is unchanged (it just runs whatever `QueryEngine` it holds).
//!
//! Scoped credentials travel INSIDE the request frame (over the pipe) — never via
//! argv/env — so a secret is never exposed in the process table or logs.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use p2p_config::{SandboxConfig, StorageConfig};
use p2p_proto::{ResultSet, ScopedCredential};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::{EngineError, ExecLease, JobContext, QueryEngine};
use crate::liveness::now_ms;
use crate::sandbox::{JobBudget, Sandbox, SandboxSpec};

/// Hard cap on a single framed request/response (anti-OOM on a garbled/hostile
/// stream), mirroring the result-stream manifest guards.
const MAX_FRAME_BYTES: u64 = 256 * 1024 * 1024;

/// The job to run, serialized to the sandboxed child's stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRequest {
    pub sql: String,
    pub memory_bytes: u64,
    pub threads: u32,
    /// Scoped, short-lived storage credential — carried in-band over the pipe.
    pub credential: Option<ScopedCredential>,
    /// At-rest Parquet key material (`name` → raw bytes).
    pub parquet_keys: Vec<(String, Vec<u8>)>,
    /// How often (ms) the child emits a [`JobProgress`] heartbeat while executing
    /// (so stall detection sees liveness under `process_per_job`). `0` ⇒ only the
    /// leading "executing" heartbeat is sent.
    pub progress_interval_ms: u64,
}

/// The child's reply: the materialized result, or a stringified engine error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResponse {
    pub result: Option<ResultSet>,
    pub error: Option<String>,
}

/// A streamed execution-progress heartbeat from the child (mirrors
/// [`p2p_proto::Progress`] for the in-process path), carried as an interim frame
/// before the final [`JobResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub stage: String,
    pub rows_processed: u64,
    pub pct: u8,
    pub seq: u32,
    pub ts_ms: u64,
}

/// One frame on the child→parent stream: zero or more [`JobProgress`] heartbeats
/// followed by exactly one terminal [`JobResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobFrame {
    Progress(JobProgress),
    Done(JobResponse),
}

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as u64;
    if n > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "job frame exceeds the maximum size",
        ));
    }
    let mut buf = vec![0u8; n as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Child side: read ONE [`JobRequest`] from `reader`, run it on `engine`, and
/// write the [`JobResponse`] to `writer`. One request per process invocation.
pub async fn serve_job<R, W>(
    engine: &dyn QueryEngine,
    reader: &mut R,
    writer: &mut W,
) -> std::io::Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let bytes = read_frame(reader).await?;
    let req: JobRequest = p2p_proto::from_bytes(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let interval_ms = req.progress_interval_ms;
    let ctx = JobContext {
        credential: req.credential,
        parquet_keys: req.parquet_keys,
    };
    let lease = ExecLease {
        memory_bytes: req.memory_bytes,
        threads: req.threads,
    };

    // Leading heartbeat: the child has started executing (deterministic liveness —
    // distinguishes "spawned but stuck" from "running"), then periodic heartbeats
    // while the engine runs, then the terminal Done frame.
    let mut seq: u32 = 1;
    let _ = emit_progress(writer, seq).await;

    let fut = engine.execute_job(&req.sql, lease, &ctx);
    tokio::pin!(fut);
    let outcome = if interval_ms > 0 {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                r = &mut fut => break r,
                _ = ticker.tick() => {
                    seq = seq.saturating_add(1);
                    let _ = emit_progress(writer, seq).await;
                }
            }
        }
    } else {
        fut.await
    };

    let resp = match outcome {
        Ok(rs) => JobResponse {
            result: Some(rs),
            error: None,
        },
        Err(e) => JobResponse {
            result: None,
            error: Some(e.to_string()),
        },
    };
    let out = p2p_proto::to_bytes(&JobFrame::Done(resp))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(writer, &out).await
}

/// Write one progress heartbeat frame; best-effort (a broken pipe during progress
/// must not abort the in-flight job — the terminal Done frame will surface the
/// real outcome or the parent will tear the child down).
async fn emit_progress<W: AsyncWriteExt + Unpin>(writer: &mut W, seq: u32) -> std::io::Result<()> {
    let frame = JobFrame::Progress(JobProgress {
        stage: "executing".into(),
        rows_processed: 0,
        pct: 0,
        seq,
        ts_ms: now_ms(),
    });
    let bytes = p2p_proto::to_bytes(&frame)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(writer, &bytes).await
}

/// A [`QueryEngine`] that runs each job in an OS-sandboxed CHILD process.
///
/// Constructed only on the opt-in `[sandbox].process_per_job` path; the worker
/// uses it transparently in place of the in-process engine, so no worker hot-path
/// code changes. The child is spawned per job and killed on drop (loser /
/// cancellation / timeout teardown is the OS killing the process).
pub struct SubprocessEngine {
    sandbox: Arc<dyn Sandbox>,
    program: OsString,
    args: Vec<OsString>,
    version: String,
    sandbox_cfg: SandboxConfig,
    storage_cfg: StorageConfig,
    /// How often the child emits a progress heartbeat (from `[worker]`); `0`
    /// disables periodic heartbeats (the leading one is still sent).
    progress_interval_ms: u64,
    /// Optional sink for child-streamed progress. `None` (default) drains the
    /// heartbeats — the worker's own progress ticker remains the requester-facing
    /// liveness signal until a per-job progress sink is threaded through the
    /// `QueryEngine` trait (see the report).
    progress_sink: Option<UnboundedSender<JobProgress>>,
}

impl SubprocessEngine {
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        program: impl Into<OsString>,
        args: Vec<OsString>,
        version: impl Into<String>,
        sandbox_cfg: SandboxConfig,
        storage_cfg: StorageConfig,
        progress_interval_ms: u64,
    ) -> Self {
        Self {
            sandbox,
            program: program.into(),
            args,
            version: version.into(),
            sandbox_cfg,
            storage_cfg,
            progress_interval_ms,
            progress_sink: None,
        }
    }

    /// Forward child-streamed [`JobProgress`] heartbeats to `sink` (default: drop).
    pub fn with_progress_sink(mut self, sink: UnboundedSender<JobProgress>) -> Self {
        self.progress_sink = Some(sink);
        self
    }

    async fn run(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        // Per-job private temp dir (spill) + the OS sandbox spec (rlimits from the
        // lease, egress/read-only from config).
        let job_tmp = std::env::temp_dir().join(format!(
            "p2p-job-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let spec = SandboxSpec::resolve(
            &self.sandbox_cfg,
            &self.storage_cfg,
            JobBudget {
                memory_bytes: lease.memory_bytes,
                threads: lease.threads,
            },
            job_tmp.to_string_lossy().to_string(),
        );
        let std_cmd = self
            .sandbox
            .command(&self.program, &self.args, &spec)
            .map_err(|e| EngineError::Rejected(format!("build sandbox command: {e}")))?;

        let mut cmd = tokio::process::Command::from(std_cmd);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            // The OS reaps a loser/cancelled/timed-out job: when the worker drops
            // this future, the child is killed instead of leaking compute.
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| EngineError::Exec(format!("spawn job executor: {e}")))?;

        let req = JobRequest {
            sql: sql.to_string(),
            memory_bytes: lease.memory_bytes,
            threads: lease.threads,
            credential: ctx.credential.clone(),
            parquet_keys: ctx.parquet_keys.clone(),
            progress_interval_ms: self.progress_interval_ms,
        };
        let bytes = p2p_proto::to_bytes(&req)
            .map_err(|e| EngineError::Exec(format!("encode job request: {e}")))?;

        // Write the request, then drop stdin so the child sees EOF and proceeds.
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| EngineError::Exec("child stdin unavailable".into()))?;
            write_frame(&mut stdin, &bytes)
                .await
                .map_err(|e| EngineError::Exec(format!("write job request: {e}")))?;
        }

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError::Exec("child stdout unavailable".into()))?;
        // Read the child→parent stream: progress heartbeats (forwarded to the sink
        // if wired, else drained) until the terminal Done frame carries the result.
        let resp = loop {
            let frame_bytes = read_frame(&mut stdout)
                .await
                .map_err(|e| EngineError::Exec(format!("read job frame: {e}")))?;
            let frame: JobFrame = p2p_proto::from_bytes(&frame_bytes)
                .map_err(|e| EngineError::Exec(format!("decode job frame: {e}")))?;
            match frame {
                JobFrame::Progress(p) => {
                    if let Some(sink) = &self.progress_sink {
                        let _ = sink.send(p);
                    }
                }
                JobFrame::Done(resp) => break resp,
            }
        };
        let _ = child.wait().await;
        let _ = std::fs::remove_dir_all(&job_tmp);

        match (resp.result, resp.error) {
            (Some(rs), _) => Ok(rs),
            (None, Some(e)) => Err(EngineError::Exec(e)),
            (None, None) => Err(EngineError::Exec("empty job response".into())),
        }
    }
}

#[async_trait]
impl QueryEngine for SubprocessEngine {
    async fn execute(&self, sql: &str, lease: ExecLease) -> Result<ResultSet, EngineError> {
        self.run(sql, lease, &JobContext::default()).await
    }

    async fn execute_job(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        self.run(sql, lease, ctx).await
    }

    fn version(&self) -> String {
        self.version.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MockEngine;

    #[tokio::test]
    async fn serve_job_round_trips_request_and_result() {
        // Drive the child-side protocol in-process over a duplex pipe (no spawn):
        // a JobRequest in, a JobResponse with the engine's result out.
        let engine = MockEngine::deterministic();
        let (mut client, server) = tokio::io::duplex(64 * 1024);

        let req = JobRequest {
            sql: "SELECT 1".into(),
            memory_bytes: 64 << 20,
            threads: 1,
            credential: None,
            parquet_keys: vec![],
            progress_interval_ms: 0,
        };
        let bytes = p2p_proto::to_bytes(&req).unwrap();
        write_frame(&mut client, &bytes).await.unwrap();

        let (mut sr, mut sw) = tokio::io::split(server);
        serve_job(&engine, &mut sr, &mut sw).await.unwrap();

        // The stream carries a leading progress heartbeat, then the terminal Done.
        let f0: JobFrame = p2p_proto::from_bytes(&read_frame(&mut client).await.unwrap()).unwrap();
        assert!(matches!(f0, JobFrame::Progress(_)), "leading heartbeat first");
        let resp = loop {
            let frame: JobFrame =
                p2p_proto::from_bytes(&read_frame(&mut client).await.unwrap()).unwrap();
            if let JobFrame::Done(r) = frame {
                break r;
            }
        };
        assert!(resp.error.is_none(), "deterministic query must succeed");
        let rs = resp.result.expect("a result set");
        // Same engine + SQL must reproduce the in-process result exactly.
        let direct = engine
            .execute("SELECT 1", ExecLease {
                memory_bytes: 64 << 20,
                threads: 1,
            })
            .await
            .unwrap();
        assert_eq!(rs, direct);
    }

    #[tokio::test]
    async fn serve_job_reports_engine_error() {
        let engine = MockEngine::failing("Out of Memory Error: boom");
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let req = JobRequest {
            sql: "SELECT 1".into(),
            memory_bytes: 1,
            threads: 1,
            credential: None,
            parquet_keys: vec![],
            progress_interval_ms: 0,
        };
        write_frame(&mut client, &p2p_proto::to_bytes(&req).unwrap())
            .await
            .unwrap();
        let (mut sr, mut sw) = tokio::io::split(server);
        serve_job(&engine, &mut sr, &mut sw).await.unwrap();
        // Drain frames to the terminal Done, which carries the engine error.
        let resp = loop {
            let frame: JobFrame =
                p2p_proto::from_bytes(&read_frame(&mut client).await.unwrap()).unwrap();
            if let JobFrame::Done(r) = frame {
                break r;
            }
        };
        assert!(resp.result.is_none());
        assert!(resp.error.unwrap().contains("Out of Memory"));
    }

    #[tokio::test]
    async fn serve_job_streams_periodic_progress_then_done() {
        // With a progress interval set and a slow engine, the child streams
        // multiple progress heartbeats before the terminal Done (so stall
        // detection sees liveness under process_per_job).
        let engine = MockEngine::deterministic().with_delay(std::time::Duration::from_millis(120));
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let req = JobRequest {
            sql: "SELECT 1".into(),
            memory_bytes: 64 << 20,
            threads: 1,
            credential: None,
            parquet_keys: vec![],
            progress_interval_ms: 20,
        };
        write_frame(&mut client, &p2p_proto::to_bytes(&req).unwrap())
            .await
            .unwrap();
        let (mut sr, mut sw) = tokio::io::split(server);
        let serve = tokio::spawn(async move { serve_job(&engine, &mut sr, &mut sw).await });

        let mut progress = 0;
        let resp = loop {
            let frame: JobFrame =
                p2p_proto::from_bytes(&read_frame(&mut client).await.unwrap()).unwrap();
            match frame {
                JobFrame::Progress(_) => progress += 1,
                JobFrame::Done(r) => break r,
            }
        };
        serve.await.unwrap().unwrap();
        assert!(resp.error.is_none());
        assert!(progress >= 2, "expected multiple heartbeats, got {progress}");
    }

    #[tokio::test]
    async fn subprocess_engine_spawn_failure_is_an_error_not_a_panic() {
        // Pointing at a non-existent executor must surface a clean EngineError
        // (the worker then treats it like any other engine failure), never panic.
        let sandbox = crate::sandbox::build(&SandboxConfig::default());
        let eng = SubprocessEngine::new(
            sandbox,
            "/nonexistent/p2p-job-exec-xyz",
            vec![],
            "duckdb-subprocess",
            SandboxConfig::default(),
            StorageConfig::default(),
            0,
        );
        let err = eng
            .execute("SELECT 1", ExecLease {
                memory_bytes: 1 << 20,
                threads: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Exec(_) | EngineError::Rejected(_)));
    }
}
