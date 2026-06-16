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

use async_trait::async_trait;
use p2p_config::{SandboxConfig, StorageConfig};
use p2p_proto::{ResultSet, ScopedCredential};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::engine::{EngineError, ExecLease, JobContext, QueryEngine};
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
}

/// The child's reply: the materialized result, or a stringified engine error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResponse {
    pub result: Option<ResultSet>,
    pub error: Option<String>,
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
    let ctx = JobContext {
        credential: req.credential,
        parquet_keys: req.parquet_keys,
    };
    let lease = ExecLease {
        memory_bytes: req.memory_bytes,
        threads: req.threads,
    };
    let resp = match engine.execute_job(&req.sql, lease, &ctx).await {
        Ok(rs) => JobResponse {
            result: Some(rs),
            error: None,
        },
        Err(e) => JobResponse {
            result: None,
            error: Some(e.to_string()),
        },
    };
    let out = p2p_proto::to_bytes(&resp)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    write_frame(writer, &out).await
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
}

impl SubprocessEngine {
    pub fn new(
        sandbox: Arc<dyn Sandbox>,
        program: impl Into<OsString>,
        args: Vec<OsString>,
        version: impl Into<String>,
        sandbox_cfg: SandboxConfig,
        storage_cfg: StorageConfig,
    ) -> Self {
        Self {
            sandbox,
            program: program.into(),
            args,
            version: version.into(),
            sandbox_cfg,
            storage_cfg,
        }
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
        let resp_bytes = read_frame(&mut stdout)
            .await
            .map_err(|e| EngineError::Exec(format!("read job response: {e}")))?;
        let _ = child.wait().await;
        let _ = std::fs::remove_dir_all(&job_tmp);

        let resp: JobResponse = p2p_proto::from_bytes(&resp_bytes)
            .map_err(|e| EngineError::Exec(format!("decode job response: {e}")))?;
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
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let req = JobRequest {
            sql: "SELECT 1".into(),
            memory_bytes: 64 << 20,
            threads: 1,
            credential: None,
            parquet_keys: vec![],
        };
        let bytes = p2p_proto::to_bytes(&req).unwrap();
        write_frame(&mut client, &bytes).await.unwrap();

        let (mut sr, mut sw) = tokio::io::split(server);
        serve_job(&engine, &mut sr, &mut sw).await.unwrap();

        let resp_bytes = read_frame(&mut client).await.unwrap();
        let resp: JobResponse = p2p_proto::from_bytes(&resp_bytes).unwrap();
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
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let req = JobRequest {
            sql: "SELECT 1".into(),
            memory_bytes: 1,
            threads: 1,
            credential: None,
            parquet_keys: vec![],
        };
        write_frame(&mut client, &p2p_proto::to_bytes(&req).unwrap())
            .await
            .unwrap();
        let (mut sr, mut sw) = tokio::io::split(server);
        serve_job(&engine, &mut sr, &mut sw).await.unwrap();
        let resp: JobResponse =
            p2p_proto::from_bytes(&read_frame(&mut client).await.unwrap()).unwrap();
        assert!(resp.result.is_none());
        assert!(resp.error.unwrap().contains("Out of Memory"));
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
