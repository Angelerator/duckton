//! Real DuckDB-backed query engine with the lockdown sandbox (architecture
//! §9.2, §9.4) plus secure remote object-storage reads (§4).
//!
//! Gated behind the `duckdb-engine` feature because it bundles + compiles DuckDB
//! from source. Each execution runs on a fresh in-memory connection set up in a
//! strict order BEFORE the untrusted query runs:
//!  1. `SET memory_limit` / `SET threads` to the granted lease,
//!  2. `SET temp_directory` to an ephemeral dir (no spill to shared disk),
//!  3. disable extension auto-install/auto-load and unsigned extensions,
//!  4. `LOAD` the pre-resolved extension set (decided at engine init from
//!     config — never from the query; no `INSTALL`/`LOAD` at query time),
//!  5. `SET allowed_directories=[…]` for any configured local fixtures
//!     (before external access is locked),
//!  6. install the per-job scoped, short-lived `CREATE SECRET` + at-rest
//!     `add_parquet_key`s,
//!  7. `SET enable_external_access` (true only when remote reads are enabled),
//!  8. `SET lock_configuration=true` (no later `SET` can re-open the sandbox).
//!
//! ## Security boundary — what DuckDB can and cannot enforce
//! * **Local files:** `allowed_directories`/`allowed_paths` give fine-grained,
//!   native restriction — even when `enable_external_access=false`. The strict
//!   engine allows none; the local-scoped engine allows only the configured
//!   fixture dirs; everything else (e.g. `/etc/passwd`) is blocked.
//! * **Network:** `enable_external_access` is **all-or-nothing** — DuckDB
//!   cannot restrict egress to specific storage endpoints. So the remote
//!   profile minimizes blast radius via **scoped, short-lived secrets** (the
//!   credential only reads its prefix, read-only, for minutes) and relies on
//!   **OS-level egress filtering** as the complementary control (the OS sandbox
//!   itself is separately deferred, §9.4).
//!
//! Execution runs on a blocking thread (DuckDB is synchronous) via
//! `spawn_blocking`, so the async runtime is not stalled.

use std::sync::Arc;

use async_trait::async_trait;
use duckdb::types::ValueRef;
use duckdb::{Connection, InterruptHandle};
use p2p_proto::{ResultSet, Value};
use tokio::sync::{oneshot, Notify};

use crate::datasource::StorageSetup;
use crate::engine::{EngineError, ExecLease, JobContext, QueryEngine};

/// A locked-down DuckDB execution engine.
pub struct DuckDbEngine {
    version: String,
    setup: StorageSetup,
}

impl DuckDbEngine {
    /// The strict engine: no network egress, no allowed dirs, no providers.
    /// Equivalent to the original locked-down engine (blocks all file/network
    /// access, `INSTALL`/`LOAD`).
    pub fn new() -> Result<Self, EngineError> {
        Self::with_setup(StorageSetup::strict())
    }

    /// Build an engine with a resolved [`StorageSetup`] (formats/providers/
    /// pre-load list / allowed dirs / remote-access switch from config).
    ///
    /// Pre-loads (verifies) the configured extensions once at init. If
    /// `require_extensions` is set and an extension cannot be loaded, init fails
    /// — honoring "required extensions must be pre-loaded at engine init".
    pub fn with_setup(mut setup: StorageSetup) -> Result<Self, EngineError> {
        let conn =
            Connection::open_in_memory().map_err(|e| EngineError::Exec(format!("open: {e}")))?;
        let version: String = conn
            .query_row("SELECT library_version FROM pragma_version()", [], |r| {
                r.get(0)
            })
            .map_err(|e| EngineError::Exec(format!("version: {e}")))?;

        // Verify the extension pre-load decision once, at init. Track whether the
        // `httpfs` crypto provider loaded: `temp_file_encryption`'s ENCRYPTED SPILL
        // WRITE needs a writable crypto module, which on this build is provided by
        // `httpfs` (mbedtls). Because the engine hardens extensions (autoload OFF),
        // that module is available at query time ONLY when `httpfs` is in
        // `preload_extensions`. `SET temp_file_encryption=true` itself is accepted
        // on a `:memory:` connection regardless, but the spill WRITE then fails
        // ("read-only crypto module") — so we gate the EFFECTIVE flag on httpfs
        // actually being loaded and disable it gracefully otherwise (plaintext
        // spill, today's behavior), rather than breaking every spilling job.
        Self::harden_extensions(&conn)?;
        let mut httpfs_loaded = false;
        for ext in &setup.preload_extensions {
            match conn.execute_batch(&format!("LOAD {ext};")) {
                Ok(()) => {
                    if ext.eq_ignore_ascii_case("httpfs") {
                        httpfs_loaded = true;
                    }
                }
                Err(e) => {
                    if setup.require_extensions {
                        return Err(EngineError::Exec(format!(
                            "preload extension '{ext}' failed at init: {e} (install it in the \
                             worker image, or set storage.require_extensions=false)"
                        )));
                    }
                    tracing::warn!("optional extension '{ext}' not loaded: {e}");
                }
            }
        }
        if setup.temp_file_encryption && !httpfs_loaded {
            tracing::warn!(
                "temp_file_encryption is enabled but the 'httpfs' crypto provider is not \
                 preloaded; on-disk spill will NOT be encrypted at rest. Add \"httpfs\" to \
                 storage.preload_extensions to encrypt spill files."
            );
            setup.temp_file_encryption = false;
        }

        Ok(Self {
            version: format!("duckdb-{version}"),
            setup,
        })
    }

    /// Build an engine directly from a storage config section.
    pub fn from_storage_config(cfg: &p2p_config::StorageConfig) -> Result<Self, EngineError> {
        Self::with_setup(StorageSetup::from_config(cfg))
    }

    /// Engine-backed pre-flight Parquet metadata probe (architecture §4 /
    /// estimator). Runs DuckDB's `parquet_metadata()` over `path` (which must be
    /// inside the engine's `allowed_directories`, or reachable when remote
    /// access is enabled) and converts the result into the estimator's
    /// [`crate::estimator::ParquetMetadata`] — footer-only, **no full scan**.
    ///
    /// `path` is a file or glob; it is single-quote escaped. The heavy lifting
    /// (grouping rows into row-groups, parsing stats) is the pure
    /// [`crate::estimator::parquet_metadata_from_resultset`], unit-tested without
    /// an engine.
    pub async fn probe_parquet_metadata(
        &self,
        path: &str,
        lease: ExecLease,
    ) -> Result<crate::estimator::ParquetMetadata, EngineError> {
        let sql = format!(
            "SELECT row_group_id, row_group_num_rows, path_in_schema, num_values, \
             total_uncompressed_size, stats_min_value, stats_max_value \
             FROM parquet_metadata('{}') ORDER BY row_group_id",
            path.replace('\'', "''")
        );
        let rs = self.execute(&sql, lease).await?;
        Ok(crate::estimator::parquet_metadata_from_resultset(&rs))
    }

    /// Engine-backed `EXPLAIN` cardinality probe: returns the maximum estimated
    /// cardinality DuckDB's optimizer assigns to any operator in the plan for
    /// `sql` (a proxy for the heaviest operator's row count). Modern DuckDB
    /// annotates operators with `~<n> rows`; legacy builds used `EC: n`. Pure
    /// parsing is [`crate::estimator::parse_explain_cardinality`].
    pub async fn probe_explain_cardinality(
        &self,
        sql: &str,
        lease: ExecLease,
    ) -> Result<Option<u64>, EngineError> {
        let rs = self.execute(&format!("EXPLAIN {sql}"), lease).await?;
        // EXPLAIN returns rows of (explain_key, explain_value) text; concatenate
        // all text cells and parse the EC annotations out of the plan.
        let mut text = String::new();
        for row in &rs.rows {
            for v in row {
                if let p2p_proto::Value::Text(s) = v {
                    text.push_str(s);
                    text.push('\n');
                }
            }
        }
        Ok(crate::estimator::parse_explain_cardinality(&text))
    }

    /// Defense-in-depth extension hardening applied before any LOAD.
    fn harden_extensions(conn: &Connection) -> Result<(), EngineError> {
        conn.execute_batch(crate::engine::EXTENSION_HARDENING_SQL)
            .map_err(|e| EngineError::Rejected(format!("extension hardening: {e}")))
    }

    fn run_locked(
        sql: &str,
        lease: ExecLease,
        setup: &StorageSetup,
        ctx: &JobContext,
        // When set, the per-job connection's interrupt handle is sent back here
        // right after the connection opens (before the untrusted query runs) so
        // an async canceller can abort a long/abandoned query (H7). `None` ⇒ no
        // cancellation wiring (own-query / tests).
        handle_tx: Option<oneshot::Sender<Arc<InterruptHandle>>>,
    ) -> Result<ResultSet, EngineError> {
        let conn =
            Connection::open_in_memory().map_err(|e| EngineError::Exec(format!("open: {e}")))?;
        // Hand the interrupt handle to the canceller BEFORE running anything, so a
        // timeout/cancel that arrives mid-query can interrupt the running statement.
        if let Some(tx) = handle_tx {
            let _ = tx.send(conn.interrupt_handle());
        }

        // 1. Budget + per-job spill (temp) dir. The worker pre-creates ONE per-job
        // dir and passes it via `ctx.spill_dir` — the SAME path it declares as the
        // OS sandbox's writable scope, so spill is genuinely confined when the
        // sandbox is active (the worker owns its lifetime/cleanup). With no
        // provided dir (own-query / tests) the engine mints its OWN private dir
        // (0700, unique name) removed when this `TempDir` drops at the end of
        // `run_locked` — never a long-lived shared per-process dir leaking spill
        // across jobs/tenants. `tempfile` creates it with `0700` on Unix.
        let mb = (lease.memory_bytes / (1024 * 1024)).max(64);
        let _owned_tmp; // keeps an engine-minted TempDir alive for this scope
        let tmp_path = match &ctx.spill_dir {
            Some(dir) => dir.replace('\'', "''"),
            None => {
                let job_tmp = tempfile::Builder::new()
                    .prefix("duckdb-p2p-job-")
                    .tempdir()
                    .map_err(|e| EngineError::Rejected(format!("temp dir: {e}")))?;
                let p = job_tmp.path().to_string_lossy().replace('\'', "''");
                _owned_tmp = job_tmp;
                p
            }
        };
        // Bound on-disk spill (DuckDB `max_temp_directory_size`) so a runaway
        // query cannot fill the host disk; `0` ⇒ unbounded (DuckDB default).
        // Encrypt spill at rest when supported (resolved at engine init). Both are
        // SET before `lock_configuration` so the untrusted query cannot widen them.
        let mut budget_sql = format!(
            "SET memory_limit='{mb}MB'; SET threads={}; SET temp_directory='{tmp_path}';",
            lease.threads.max(1),
        );
        if setup.temp_file_encryption {
            budget_sql.push_str(" SET temp_file_encryption=true;");
        }
        if lease.max_spill_bytes > 0 {
            budget_sql.push_str(&format!(
                " SET max_temp_directory_size='{}B';",
                lease.max_spill_bytes
            ));
        }
        conn.execute_batch(&budget_sql)
            .map_err(|e| EngineError::Rejected(format!("budget setup: {e}")))?;

        // 2-4. Harden + pre-load the resolved extension set (never the query's
        // choice; no INSTALL — only LOAD of already-available extensions).
        Self::harden_extensions(&conn)?;
        for ext in &setup.preload_extensions {
            if let Err(e) = conn.execute_batch(&format!("LOAD {ext};")) {
                if setup.require_extensions {
                    return Err(EngineError::Rejected(format!("LOAD {ext}: {e}")));
                }
            }
        }

        // 5. Allow-list local fixture directories (must precede locking external
        // access). This is the native, fine-grained local-FS restriction.
        let strict_local = setup.allowed_local_paths.is_empty();
        if !strict_local {
            let list = setup
                .allowed_local_paths
                .iter()
                .map(|p| format!("'{}'", p.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute_batch(&format!("SET allowed_directories=[{list}];"))
                .map_err(|e| EngineError::Rejected(format!("allowed_directories: {e}")))?;
        }

        // 6. Per-job at-rest keys + scoped, short-lived storage secret.
        for (name, key) in &ctx.parquet_keys {
            let key_lit = String::from_utf8_lossy(key).replace('\'', "''");
            conn.execute_batch(&format!(
                "PRAGMA add_parquet_key('{}', '{}');",
                name.replace('\'', "''"),
                key_lit
            ))
            .map_err(|e| EngineError::Rejected(format!("add_parquet_key: {e}")))?;
        }
        if let Some(cred) = &ctx.credential {
            // Open a sealed credential just-in-time with the worker's sealing
            // key (no-op for plaintext tokens). The decrypted key material lives
            // only in this transient value and the generated SQL is never logged.
            let cred = setup
                .resolve_credential(cred)
                .map_err(|e| EngineError::Rejected(format!("credential: {e}")))?;
            match setup.providers.secret_sql_for("job_secret", &cred) {
                Ok(Some(secret_sql)) => {
                    conn.execute_batch(&secret_sql)
                        .map_err(|e| EngineError::Rejected(format!("create secret: {e}")))?;
                }
                Ok(None) => {}
                Err(e) => return Err(EngineError::Rejected(format!("credential: {e}"))),
            }
        }

        // 7. Network egress: enabled ONLY in the remote profile. DuckDB cannot
        // scope this to specific endpoints (see module docs) — OS egress
        // filtering is the complementary control.
        let external = setup.enable_remote_access;
        conn.execute_batch(&format!("SET enable_external_access={external};"))
            .map_err(|e| EngineError::Rejected(format!("external access: {e}")))?;

        // 7b. Strict profile (NO local fixtures configured): disable the local
        // filesystem entirely — defense-in-depth on top of
        // `enable_external_access=false`. Applied AFTER the temp_directory and
        // external-access SETs (which validate the local FS): setting it earlier
        // makes those steps fail. The `temp_directory`/spill path is exempt from
        // `disabled_filesystems`, so spilling still works. NOT applied when
        // fixtures ARE allowed (it would block reading them — `allowed_directories`
        // is the fine-grained control there).
        if strict_local {
            conn.execute_batch("SET disabled_filesystems='LocalFileSystem';")
                .map_err(|e| EngineError::Rejected(format!("disabled_filesystems: {e}")))?;
        }

        // 7c. Secret hygiene: refuse to expose unredacted secrets so the
        // untrusted query can never read the job's OWN scoped cloud credential
        // via `duckdb_secrets(redact:=false)`. Locked below so it can't be
        // re-enabled. Shared verbatim with the extension's `HostEngine`.
        conn.execute_batch(crate::engine::DENY_UNREDACTED_SECRETS_SQL)
            .map_err(|e| EngineError::Rejected(format!("redact secrets: {e}")))?;

        // 8. Lock the configuration so the untrusted query can't widen anything.
        conn.execute_batch(crate::engine::LOCK_CONFIGURATION_SQL)
            .map_err(|e| EngineError::Rejected(format!("lock configuration: {e}")))?;

        // Presigned credential mode: rewrite the pinned object references to the
        // requester-signed HTTPS URLs so the read goes over plain HTTPS with NO
        // secret installed (no `CREATE SECRET` ran above when `credential` is
        // absent). Empty `signed_inputs` ⇒ the SQL is unchanged.
        let rewritten;
        let sql: &str = if ctx.signed_inputs.is_empty() {
            sql
        } else {
            rewritten = crate::datasource::rewrite_signed_urls(sql, &ctx.signed_inputs);
            &rewritten
        };

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EngineError::Exec(format!("prepare: {e}")))?;

        // Execute first; column metadata is only valid after execution.
        let mut rows = stmt
            .query([])
            .map_err(|e| EngineError::Exec(format!("query: {e}")))?;
        let columns: Vec<String> = rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
        let column_count = columns.len();

        // DoS backstop: bound how much we pull into memory. DuckDB streams rows,
        // but we materialize the whole result to hash + stream it, so an
        // unbounded result (`range(1e12)`, wide cross joins) would otherwise grow
        // this buffer without limit. Reject once the running estimate exceeds the
        // configured ceiling rather than OOMing the host. `0` ⇒ no cap.
        let cap = setup.max_result_bytes;
        let mut materialized: u64 = 0;
        let mut rows_out: Vec<Vec<Value>> = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| EngineError::Exec(format!("fetch: {e}")))?
        {
            let mut out_row = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let v = row
                    .get_ref(i)
                    .map_err(|e| EngineError::Exec(format!("get col {i}: {e}")))?;
                let value = value_from_ref(v);
                materialized = materialized.saturating_add(value_estimated_size(&value) as u64);
                out_row.push(value);
            }
            if cap != 0 && materialized > cap {
                return Err(EngineError::Exec(format!(
                    "result exceeds max_result_bytes ({cap}): query produces too large a result to materialize"
                )));
            }
            rows_out.push(out_row);
        }
        Ok(ResultSet::new(columns, rows_out))
    }
}

/// A cheap in-memory size estimate for a materialized [`Value`], used only as a
/// running bound for the result-size DoS backstop (not an exact wire size).
fn value_estimated_size(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 8,
        Value::Text(s) => s.len() + 1,
        Value::Blob(b) => b.len(),
    }
}

/// Map a DuckDB value reference into the portable [`Value`] model.
fn value_from_ref(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::Int(i as i64),
        ValueRef::SmallInt(i) => Value::Int(i as i64),
        ValueRef::Int(i) => Value::Int(i as i64),
        ValueRef::BigInt(i) => Value::Int(i),
        // 128-bit / unsigned-64 values map to the portable `Int` when they fit
        // losslessly in i64 (e.g. `sum()` of integers yields HUGEINT); only
        // genuinely out-of-range magnitudes fall back to text to avoid truncation.
        ValueRef::HugeInt(i) => i64::try_from(i)
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Text(i.to_string())),
        ValueRef::UTinyInt(i) => Value::Int(i as i64),
        ValueRef::USmallInt(i) => Value::Int(i as i64),
        ValueRef::UInt(i) => Value::Int(i as i64),
        ValueRef::UBigInt(i) => i64::try_from(i)
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Text(i.to_string())),
        ValueRef::Float(f) => Value::Float(f as f64),
        ValueRef::Double(f) => Value::Float(f),
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).to_string()),
        ValueRef::Blob(bytes) => Value::Blob(bytes.to_vec()),
        // Decimals/timestamps/etc.: render via debug for determinism.
        other => Value::Text(format!("{other:?}")),
    }
}

#[async_trait]
impl QueryEngine for DuckDbEngine {
    async fn execute(&self, sql: &str, lease: ExecLease) -> Result<ResultSet, EngineError> {
        self.execute_job(sql, lease, &JobContext::default()).await
    }

    async fn execute_job(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        let sql = sql.to_string();
        let setup = self.setup.clone();
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || {
            DuckDbEngine::run_locked(&sql, lease, &setup, &ctx, None)
        })
        .await
        .map_err(|e| EngineError::Exec(format!("join: {e}")))?
    }

    async fn execute_job_cancellable(
        &self,
        sql: &str,
        lease: ExecLease,
        ctx: &JobContext,
        cancel: Arc<Notify>,
    ) -> Result<ResultSet, EngineError> {
        let sql = sql.to_string();
        let setup = self.setup.clone();
        let ctx = ctx.clone();
        // The blocking job sends its interrupt handle back as soon as the
        // connection opens; a small bridge task waits for `cancel` and interrupts
        // the running statement so an abandoned/over-deadline query stops burning
        // a blocking thread instead of finishing (H7).
        let (htx, hrx) = oneshot::channel::<Arc<InterruptHandle>>();
        let bridge = tokio::spawn(async move {
            if let Ok(handle) = hrx.await {
                cancel.notified().await;
                handle.interrupt();
            }
        });
        let out = tokio::task::spawn_blocking(move || {
            DuckDbEngine::run_locked(&sql, lease, &setup, &ctx, Some(htx))
        })
        .await
        .map_err(|e| EngineError::Exec(format!("join: {e}")));
        // The query finished (or errored/was interrupted): tear down the bridge so
        // it never lingers when no cancellation arrived.
        bridge.abort();
        out?
    }

    fn version(&self) -> String {
        self.version.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> ExecLease {
        ExecLease {
            memory_bytes: 256 * 1024 * 1024,
            threads: 1,
            max_spill_bytes: 0,
        }
    }

    #[tokio::test]
    async fn runs_simple_query() {
        let eng = DuckDbEngine::new().unwrap();
        let rs = eng
            .execute("SELECT 1 AS a, 'hi' AS b", lease())
            .await
            .unwrap();
        assert_eq!(rs.columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(rs.rows.len(), 1);
        assert_eq!(rs.rows[0][0], Value::Int(1));
        assert_eq!(rs.rows[0][1], Value::Text("hi".into()));
    }

    #[tokio::test]
    async fn generate_series_is_deterministic() {
        let eng = DuckDbEngine::new().unwrap();
        let a = eng
            .execute("SELECT * FROM generate_series(1,100)", lease())
            .await
            .unwrap();
        let b = eng
            .execute("SELECT * FROM generate_series(1,100)", lease())
            .await
            .unwrap();
        assert_eq!(p2p_trust::canonical_hash(&a), p2p_trust::canonical_hash(&b));
        assert_eq!(a.row_count(), 100);
    }

    #[tokio::test]
    async fn lockdown_blocks_local_file_read() {
        let eng = DuckDbEngine::new().unwrap();
        // enable_external_access=false must block reading local files.
        let r = eng
            .execute("SELECT * FROM read_csv_auto('/etc/passwd')", lease())
            .await;
        assert!(r.is_err(), "local file read should be blocked, got {r:?}");
    }

    #[tokio::test]
    async fn lockdown_blocks_install_load() {
        let eng = DuckDbEngine::new().unwrap();
        let r = eng.execute("INSTALL httpfs", lease()).await;
        assert!(r.is_err(), "INSTALL should be blocked under lockdown");
    }

    #[tokio::test]
    async fn lockdown_blocks_community_extensions() {
        // `allow_community_extensions=false` + locked config: installing from the
        // community repository must be refused.
        let eng = DuckDbEngine::new().unwrap();
        let r = eng.execute("INSTALL faker FROM community", lease()).await;
        assert!(r.is_err(), "community extension install should be blocked");
    }

    #[tokio::test]
    async fn lockdown_cannot_be_reopened_by_query() {
        // `lock_configuration=true` means an untrusted query cannot re-enable
        // external access to read a local file.
        let eng = DuckDbEngine::new().unwrap();
        let r = eng
            .execute(
                "SET enable_external_access=true; SELECT * FROM read_csv_auto('/etc/passwd')",
                lease(),
            )
            .await;
        assert!(
            r.is_err(),
            "locked config must reject re-opening external access"
        );
    }

    #[tokio::test]
    async fn lockdown_blocks_unredacting_secrets() {
        // Secret hygiene (§9.2): the untrusted query must not be able to re-enable
        // unredacted secret introspection to read its OWN scoped cloud credential.
        // `allow_unredacted_secrets=false` + the locked config makes this fail.
        let eng = DuckDbEngine::new().unwrap();
        let r = eng
            .execute("SET allow_unredacted_secrets=true; SELECT 1", lease())
            .await;
        assert!(
            r.is_err(),
            "locked config must reject re-enabling unredacted secrets, got {r:?}"
        );
    }

    fn spill_lease(memory_bytes: u64, max_spill_bytes: u64) -> ExecLease {
        ExecLease {
            memory_bytes,
            threads: 2,
            max_spill_bytes,
        }
    }

    /// A LOCAL-SCOPED engine (an allowed dir ⇒ the local filesystem is NOT
    /// disabled), the only profile where DuckDB can actually spill to disk — the
    /// same profile `tpch_spill` exercises. The strict profile deliberately
    /// disables `LocalFileSystem` entirely (no spill at all), so spill hardening
    /// only applies to the file-backed profiles. `preload` lets a test pre-load
    /// the `httpfs` crypto provider that encrypted spill requires.
    fn local_scoped_engine(preload: &[&str]) -> DuckDbEngine {
        let mut setup = StorageSetup::strict();
        setup.allowed_local_paths = vec![std::env::temp_dir().to_string_lossy().to_string()];
        setup.preload_extensions = preload.iter().map(|s| s.to_string()).collect();
        DuckDbEngine::with_setup(setup).unwrap()
    }

    async fn current_temp_file_encryption(eng: &DuckDbEngine) -> Value {
        eng.execute(
            "SELECT current_setting('temp_file_encryption') AS e",
            lease(),
        )
        .await
        .expect("reading temp_file_encryption must succeed on :memory:")
        .rows[0][0]
            .clone()
    }

    /// A high-cardinality GROUP BY whose hash table far exceeds a tight
    /// memory_limit, forcing DuckDB to spill to its `temp_directory`. `n` equals
    /// the number of distinct groups, a stable correctness anchor.
    const SPILL_SQL: &str =
        "SELECT count(*) AS n FROM (SELECT i FROM range(10000000) t(i) GROUP BY i)";

    #[tokio::test]
    async fn temp_file_encryption_accepted_on_memory_but_gated_on_crypto_provider() {
        // VERIFICATION (P0 #2): `SET temp_file_encryption=true` is ACCEPTED on a
        // `:memory:` DuckDB 1.5.4 connection, but the ENCRYPTED SPILL WRITE needs
        // the `httpfs` crypto provider (the engine hardens autoload off). Without
        // it preloaded, the engine GRACEFULLY DISABLES encryption (plaintext
        // spill, today's behavior) rather than breaking — current_setting reads
        // back `false`, and a larger-than-memory query still spills+succeeds.
        let eng = local_scoped_engine(&[]);
        assert_eq!(
            current_temp_file_encryption(&eng).await,
            Value::Bool(false),
            "without the httpfs crypto provider, encryption must be gracefully disabled"
        );
        let rs = eng
            .execute(SPILL_SQL, spill_lease(64 * 1024 * 1024, 0))
            .await
            .expect("larger-than-memory query must still spill+succeed (plaintext fallback)");
        assert_eq!(rs.rows[0][0], Value::Int(10_000_000));
    }

    #[tokio::test]
    async fn encrypted_spill_succeeds_when_httpfs_preloaded() {
        // VERIFICATION (P0 #2): with the `httpfs` crypto provider preloaded,
        // temp_file_encryption is ENABLED on the `:memory:` connection AND a
        // larger-than-memory query completes via the ENCRYPTED on-disk spill.
        let eng = local_scoped_engine(&["httpfs"]);
        if current_temp_file_encryption(&eng).await != Value::Bool(true) {
            eprintln!("SKIP encrypted_spill_succeeds: httpfs unavailable in this environment");
            return;
        }
        let rs = eng
            .execute(SPILL_SQL, spill_lease(64 * 1024 * 1024, 0))
            .await
            .expect("larger-than-memory query must spill+succeed WITH encryption on");
        assert_eq!(
            rs.rows[0][0],
            Value::Int(10_000_000),
            "encrypted spilled aggregation must still be correct"
        );
    }

    #[tokio::test]
    async fn over_cap_spill_is_rejected_by_max_temp_directory_size() {
        // VERIFICATION (P0 #1): a larger-than-memory query under a TINY on-disk
        // spill cap must be REJECTED by `max_temp_directory_size` (a bounded
        // error) rather than filling the disk — exercised on the ENCRYPTED path
        // when httpfs is available, else the plaintext fallback. Either way this
        // proves the query genuinely spills (it errors only because it hit the cap).
        let eng = local_scoped_engine(&["httpfs"]);
        let r = eng
            .execute(SPILL_SQL, spill_lease(64 * 1024 * 1024, 16 * 1024 * 1024))
            .await;
        let err = r.expect_err("over-cap spill must be rejected, not allowed to fill disk");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("temp") || msg.contains("temporary") || msg.contains("disk"),
            "error should reference the temp-directory size cap, got: {msg}"
        );
    }

    #[tokio::test]
    async fn temp_directory_uses_the_provided_spill_dir() {
        // P0 #3: when the worker provides a per-job spill dir (the SAME path it
        // declares as the OS sandbox's writable scope), the engine sets
        // `temp_directory` to exactly that path — so spill is confined to it.
        let eng = DuckDbEngine::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        let ctx = JobContext {
            spill_dir: Some(path.clone()),
            ..Default::default()
        };
        let rs = eng
            .execute_job(
                "SELECT current_setting('temp_directory') AS t",
                lease(),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            rs.rows[0][0],
            Value::Text(path),
            "temp_directory must equal the worker-provided (sandbox-writable) dir"
        );
    }

    #[tokio::test]
    async fn allowed_local_paths_still_readable() {
        // When local fixture paths ARE configured, the local filesystem must NOT
        // be disabled (otherwise fixtures could not be read). Write a CSV inside a
        // permitted directory and confirm the strict-but-scoped engine reads it.
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("fixture.csv");
        std::fs::write(&csv, "a,b\n1,hi\n2,yo\n").unwrap();

        let mut setup = StorageSetup::strict();
        setup.allowed_local_paths = vec![dir.path().to_string_lossy().to_string()];
        let eng = DuckDbEngine::with_setup(setup).unwrap();

        let sql = format!(
            "SELECT count(*) AS n FROM read_csv_auto('{}')",
            csv.display().to_string().replace('\'', "''")
        );
        let rs = eng.execute(&sql, lease()).await.unwrap();
        assert_eq!(
            rs.rows[0][0],
            Value::Int(2),
            "fixture should be readable: {rs:?}"
        );
    }
}
