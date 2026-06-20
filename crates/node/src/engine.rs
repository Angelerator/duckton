//! Query execution engine abstraction.
//!
//! [`QueryEngine`] is pluggable so the protocol/scheduler can be tested
//! deterministically with [`MockEngine`] while a real DuckDB-backed engine
//! (locked-down, budgeted) is provided separately behind a feature flag.
//!
//! Determinism matters: redundant honest workers must produce identical results
//! for the same `(sql, engine_version)` so their canonical hashes agree.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use p2p_proto::{ResultSet, Value};

/// DuckDB extension-hardening PRAGMAs applied before any LOAD: no auto-install,
/// no auto-load of known extensions, and no community/unsigned extensions. Shared
/// verbatim by the node's strict `DuckDbEngine` and the extension's in-process
/// `HostEngine` so this security-critical lockdown fragment cannot drift between
/// the two engines.
pub const EXTENSION_HARDENING_SQL: &str = "SET autoinstall_known_extensions=false; \
     SET autoload_known_extensions=false; \
     SET allow_community_extensions=false; \
     SET allow_unsigned_extensions=false;";

/// Refuse to expose unredacted secrets so an untrusted query can never read the
/// job's OWN scoped cloud credential via `duckdb_secrets(redact:=false)`. Once
/// the configuration is locked this cannot be re-enabled by the query. Shared by
/// both engines so the secret-hygiene control cannot drift.
pub const DENY_UNREDACTED_SECRETS_SQL: &str = "SET allow_unredacted_secrets=false;";

/// Lock the configuration so the untrusted query cannot re-open any part of the
/// sandbox with a later `SET`. Always applied LAST.
pub const LOCK_CONFIGURATION_SQL: &str = "SET lock_configuration=true;";

/// The full STRICT in-memory lockdown applied (after the per-job budget +
/// ephemeral `temp_directory` + [`EXTENSION_HARDENING_SQL`]) to an engine that
/// opens NO local fixtures and has NO remote access: deny all external (network
/// + remote-file) access, disable the local filesystem entirely, refuse to
/// expose unredacted secrets, and lock the configuration. Shared VERBATIM by the
/// extension's in-process `HostEngine` and the node's strict `DuckDbEngine` so
/// this security-critical lockdown cannot drift between the two engines. The
/// secure-read `DuckDbEngine` (which may enable remote access / fixtures)
/// composes the same trailing pieces from [`DENY_UNREDACTED_SECRETS_SQL`] +
/// [`LOCK_CONFIGURATION_SQL`].
pub const STRICT_LOCKDOWN_SQL: &str = "SET enable_external_access=false; \
     SET disabled_filesystems='LocalFileSystem'; \
     SET allow_unredacted_secrets=false; \
     SET lock_configuration=true;";

/// Errors from query execution.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("execution failed: {0}")]
    Exec(String),
    #[error("query rejected by lockdown policy: {0}")]
    Rejected(String),
}

/// A lease describing the resource budget granted for one execution.
#[derive(Debug, Clone, Copy)]
pub struct ExecLease {
    pub memory_bytes: u64,
    pub threads: u32,
}

/// Per-job execution context delivered with a `Dispatch`: the scoped storage
/// credential (turned into a short-lived, prefix-scoped DuckDB secret by the
/// real engine) and any at-rest decryption keys. Carrying it separately from
/// `sql` keeps the secure-read wiring out of the (untrusted) query text.
#[derive(Debug, Clone, Default)]
pub struct JobContext {
    /// Scoped, short-lived storage credential (architecture §9.2).
    pub credential: Option<p2p_proto::ScopedCredential>,
    /// Named Parquet Modular Encryption keys (`name` -> raw key bytes) made
    /// available to the connection via `PRAGMA add_parquet_key` (architecture
    /// §9.2 at-rest). Released per job (e.g. opened from a sealed key).
    pub parquet_keys: Vec<(String, Vec<u8>)>,
    /// The pinned input snapshot for this job (deterministic-input verification).
    /// The real engine uses it to read the pinned object VERSIONS (e.g. an S3
    /// `versionId` passed via the secret/URL); the mock engine ignores it.
    /// `None` ⇒ no pin (today's behavior).
    pub input_snapshot: Option<p2p_proto::InputSnapshot>,
    /// **Presigned credential mode.** Per-object presigned read URLs: the real
    /// engine rewrites the SQL's object references to these signed HTTPS URLs and
    /// reads them with NO `CREATE SECRET` (no reusable secret on the host). Empty
    /// ⇒ the secret-based path via [`Self::credential`] (today's behavior).
    pub signed_inputs: Vec<p2p_proto::SignedInput>,
}

impl JobContext {
    pub fn is_empty(&self) -> bool {
        self.credential.is_none()
            && self.parquet_keys.is_empty()
            && self.input_snapshot.is_none()
            && self.signed_inputs.is_empty()
    }
}

/// Pluggable query engine.
#[async_trait]
pub trait QueryEngine: Send + Sync {
    /// Execute `sql` under the given lease, returning a materialized result.
    async fn execute(&self, sql: &str, lease: ExecLease) -> Result<ResultSet, EngineError>;

    /// Execute `sql` with a per-job context (scoped credential / at-rest keys).
    ///
    /// Default implementation ignores the context and delegates to
    /// [`QueryEngine::execute`], so mock/test engines need not implement it. The
    /// real DuckDB engine overrides this to install per-job scoped secrets.
    async fn execute_job(
        &self,
        sql: &str,
        lease: ExecLease,
        _ctx: &JobContext,
    ) -> Result<ResultSet, EngineError> {
        self.execute(sql, lease).await
    }

    /// Engine version string (folded into the query hash for determinism).
    fn version(&self) -> String;
}

/// A deterministic in-memory engine for tests.
///
/// * Without a fixture, results are derived deterministically from the SQL text
///   (so honest replicas agree).
/// * Fixtures pin specific SQL to specific results.
/// * `delay` simulates a slow worker (for hedging/racing tests).
/// * `perturb` simulates a cheater returning a wrong-but-plausible answer.
#[derive(Clone)]
pub struct MockEngine {
    version: String,
    fixtures: Arc<HashMap<String, ResultSet>>,
    delay: Duration,
    perturb: bool,
    /// When set, every `execute` returns `EngineError::Exec(msg)` — used to
    /// simulate a mid-flight failure (e.g. an out-of-memory blow-up) so the
    /// planner's adaptive fail-over can be exercised deterministically.
    fail_with: Option<String>,
}

impl Default for MockEngine {
    fn default() -> Self {
        Self::deterministic()
    }
}

impl MockEngine {
    pub fn deterministic() -> Self {
        Self {
            version: "mock-1".to_string(),
            fixtures: Arc::new(HashMap::new()),
            delay: Duration::ZERO,
            perturb: false,
            fail_with: None,
        }
    }

    /// An engine that always fails with the given message (simulates a
    /// resource-exhaustion / OOM blow-up for adaptive fail-over tests).
    pub fn failing(msg: impl Into<String>) -> Self {
        Self {
            fail_with: Some(msg.into()),
            ..Self::deterministic()
        }
    }

    pub fn with_fixtures(fixtures: HashMap<String, ResultSet>) -> Self {
        Self {
            fixtures: Arc::new(fixtures),
            ..Self::deterministic()
        }
    }

    /// Add an artificial execution delay (slow worker).
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Make this engine return a corrupted result (a cheating worker).
    pub fn cheating(mut self) -> Self {
        self.perturb = true;
        self
    }

    /// Derive a deterministic result set from the SQL text.
    fn derive(&self, sql: &str) -> ResultSet {
        let hash = blake3::hash(sql.as_bytes());
        let bytes = hash.as_bytes();
        let rows: Vec<Vec<Value>> = (0..3u8)
            .map(|i| vec![Value::Int(i as i64), Value::Int(bytes[i as usize] as i64)])
            .collect();
        ResultSet::new(vec!["k".into(), "v".into()], rows)
    }
}

#[async_trait]
impl QueryEngine for MockEngine {
    async fn execute(&self, sql: &str, _lease: ExecLease) -> Result<ResultSet, EngineError> {
        if let Some(msg) = &self.fail_with {
            return Err(EngineError::Exec(msg.clone()));
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let mut rs = match self.fixtures.get(sql) {
            Some(fixture) => fixture.clone(),
            None => self.derive(sql),
        };
        if self.perturb {
            // Corrupt the result so its canonical hash diverges from honest peers.
            rs.rows
                .push(vec![Value::Int(-1), Value::Text("tampered".into())]);
        }
        Ok(rs)
    }

    fn version(&self) -> String {
        self.version.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_trust::canonical_hash;

    fn lease() -> ExecLease {
        ExecLease {
            memory_bytes: 1 << 20,
            threads: 1,
        }
    }

    #[test]
    fn strict_lockdown_bundles_the_shared_secret_and_lock_statements() {
        // Drift guard: the strict bundle must contain the same secret-hygiene and
        // configuration-lock statements the secure-read engine applies à la carte,
        // so the two engines can never diverge on these security-critical SETs.
        assert!(STRICT_LOCKDOWN_SQL.contains(DENY_UNREDACTED_SECRETS_SQL));
        assert!(STRICT_LOCKDOWN_SQL.contains(LOCK_CONFIGURATION_SQL));
        assert!(STRICT_LOCKDOWN_SQL.contains("SET enable_external_access=false;"));
        assert!(STRICT_LOCKDOWN_SQL.contains("SET disabled_filesystems='LocalFileSystem';"));
    }

    #[tokio::test]
    async fn deterministic_engine_agrees_with_itself() {
        let e1 = MockEngine::deterministic();
        let e2 = MockEngine::deterministic();
        let r1 = e1.execute("SELECT 1", lease()).await.unwrap();
        let r2 = e2.execute("SELECT 1", lease()).await.unwrap();
        assert_eq!(canonical_hash(&r1), canonical_hash(&r2));
    }

    #[tokio::test]
    async fn cheating_engine_diverges() {
        let honest = MockEngine::deterministic();
        let cheat = MockEngine::deterministic().cheating();
        let rh = honest.execute("SELECT 1", lease()).await.unwrap();
        let rc = cheat.execute("SELECT 1", lease()).await.unwrap();
        assert_ne!(canonical_hash(&rh), canonical_hash(&rc));
    }
}
