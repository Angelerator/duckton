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

use async_trait::async_trait;
use duckdb::types::ValueRef;
use duckdb::Connection;
use p2p_proto::{ResultSet, Value};

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
    pub fn with_setup(setup: StorageSetup) -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| EngineError::Exec(format!("open: {e}")))?;
        let version: String = conn
            .query_row("SELECT library_version FROM pragma_version()", [], |r| r.get(0))
            .map_err(|e| EngineError::Exec(format!("version: {e}")))?;

        // Verify the extension pre-load decision once, at init.
        Self::harden_extensions(&conn)?;
        for ext in &setup.preload_extensions {
            if let Err(e) = conn.execute_batch(&format!("LOAD {ext};")) {
                if setup.require_extensions {
                    return Err(EngineError::Exec(format!(
                        "preload extension '{ext}' failed at init: {e} (install it in the \
                         worker image, or set storage.require_extensions=false)"
                    )));
                }
                tracing::warn!("optional extension '{ext}' not loaded: {e}");
            }
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
        conn.execute_batch(
            "SET autoinstall_known_extensions=false; \
             SET autoload_known_extensions=false; \
             SET allow_community_extensions=false; \
             SET allow_unsigned_extensions=false;",
        )
        .map_err(|e| EngineError::Rejected(format!("extension hardening: {e}")))
    }

    fn run_locked(
        sql: &str,
        lease: ExecLease,
        setup: &StorageSetup,
        ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| EngineError::Exec(format!("open: {e}")))?;

        // 1. Budget + ephemeral temp dir. Each job gets its OWN private temp dir
        // (0700, unique name) that is removed when this `TempDir` drops at the end
        // of `run_locked` — never a long-lived shared per-process dir that would
        // leak spill files across jobs (and across tenants). `tempfile` creates it
        // with `0700` on Unix.
        let mb = (lease.memory_bytes / (1024 * 1024)).max(64);
        let job_tmp = tempfile::Builder::new()
            .prefix("duckdb-p2p-job-")
            .tempdir()
            .map_err(|e| EngineError::Rejected(format!("temp dir: {e}")))?;
        let tmp_path = job_tmp.path().to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!(
            "SET memory_limit='{mb}MB'; SET threads={}; SET temp_directory='{tmp_path}';",
            lease.threads.max(1),
        ))
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

        // 8. Lock the configuration so the untrusted query can't widen anything.
        conn.execute_batch("SET lock_configuration=true;")
            .map_err(|e| EngineError::Rejected(format!("lock configuration: {e}")))?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| EngineError::Exec(format!("prepare: {e}")))?;

        // Execute first; column metadata is only valid after execution.
        let mut rows = stmt
            .query([])
            .map_err(|e| EngineError::Exec(format!("query: {e}")))?;
        let columns: Vec<String> = rows
            .as_ref()
            .map(|s| s.column_names())
            .unwrap_or_default();
        let column_count = columns.len();

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
                out_row.push(value_from_ref(v));
            }
            rows_out.push(out_row);
        }
        Ok(ResultSet::new(columns, rows_out))
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
        ValueRef::HugeInt(i) => {
            i64::try_from(i).map(Value::Int).unwrap_or_else(|_| Value::Text(i.to_string()))
        }
        ValueRef::UTinyInt(i) => Value::Int(i as i64),
        ValueRef::USmallInt(i) => Value::Int(i as i64),
        ValueRef::UInt(i) => Value::Int(i as i64),
        ValueRef::UBigInt(i) => {
            i64::try_from(i).map(Value::Int).unwrap_or_else(|_| Value::Text(i.to_string()))
        }
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
        tokio::task::spawn_blocking(move || DuckDbEngine::run_locked(&sql, lease, &setup, &ctx))
            .await
            .map_err(|e| EngineError::Exec(format!("join: {e}")))?
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
        let r = eng
            .execute("INSTALL faker FROM community", lease())
            .await;
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
        assert!(r.is_err(), "locked config must reject re-opening external access");
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
        assert_eq!(rs.rows[0][0], Value::Int(2), "fixture should be readable: {rs:?}");
    }
}
