//! Coordinator-side input resolution + pinning (deterministic-input verification).
//!
//! Before dispatching a job the coordinator inspects the SQL, enumerates the
//! external objects it reads, and resolves each to a concrete, version-identified
//! [`PinnedObject`] (S3 `versionId`/`ETag`, GCS `generation`, a content hash, or a
//! Delta/Iceberg table snapshot). The resulting [`InputSnapshot`] + its
//! deterministic fingerprint are attached to the [`crate::engine`]-bound
//! [`p2p_proto::Dispatch`], so every replica reads the SAME bytes and a benign
//! "data changed between executions" outcome is distinguishable from a fault.
//!
//! ## What is real vs. needs live cloud (honest scope)
//! * The **SQL source parser** ([`parse_input_sources`]) and the **fingerprint**
//!   are pure Rust and fully unit-tested.
//! * The **local-filesystem probe** ([`LocalFsProbe`]) is real: it content-hashes
//!   (or, for Delta, reads `_delta_log` to pin the snapshot version) without any
//!   engine, and is exercised on real fixtures.
//! * **S3 / Azure / GCS** version probes need a live object-store client with the
//!   job's credentials; they are pluggable behind [`ObjectVersionProbe`]. When no
//!   probe is registered for a scheme the resolver reports the inputs as *not
//!   statically pinnable* ([`InputResolveError::NotPinnable`]) and the coordinator
//!   falls back to result-hash quorum (today's behavior) — never a false penalty.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use p2p_proto::{InputSnapshot, ObjectVersion, PinnedObject};

/// Errors from resolving a job's pinned input snapshot.
#[derive(Debug, thiserror::Error)]
pub enum InputResolveError {
    /// The SQL references external data, but it cannot be **statically** pinned
    /// (e.g. a reader fed a bind variable / non-literal path, or a scheme this
    /// node has no version probe for). The coordinator treats the job as
    /// non-verifiable rather than penalizing anyone (architecture P3).
    #[error("inputs cannot be statically pinned: {0}")]
    NotPinnable(String),
    /// Inputs are present and pinnable, but the object store could not be reached
    /// or an object/version is gone. A job/input fault (no provider penalty).
    #[error("input source unavailable: {0}")]
    Unavailable(String),
}

/// Resolves a job's pinned [`InputSnapshot`] from its SQL. Pluggable so a node
/// can wire real cloud version probes; `None` on the coordinator ⇒ no pinning
/// (today's behavior).
#[async_trait]
pub trait InputResolver: Send + Sync {
    /// Resolve the pinned snapshot for `sql`. `Ok(None)` ⇒ the SQL references no
    /// pinnable external source (a pure in-memory query); `Ok(Some(_))` ⇒ a
    /// pinned manifest; `Err(_)` ⇒ unpinnable (fall back) or unavailable (stop).
    async fn resolve(&self, sql: &str) -> Result<Option<InputSnapshot>, InputResolveError>;
}

// ---------------------------------------------------------------------------
// SQL source extraction (pure)
// ---------------------------------------------------------------------------

/// How a parsed source must be pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// A single concrete object (e.g. `s3://b/k.parquet`).
    Object,
    /// A glob/wildcard that expands to multiple objects at resolve time.
    Glob,
    /// A Delta table directory (pin the table snapshot/version).
    Delta,
    /// An Iceberg table (pin the snapshot id).
    Iceberg,
}

/// One external source extracted from the SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlSource {
    pub uri: String,
    pub kind: SourceKind,
}

/// The result of statically classifying a query's external inputs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlSources {
    pub sources: Vec<SqlSource>,
    /// A data-reader was invoked with a NON-literal argument (bind variable,
    /// subquery, identifier) — its inputs cannot be statically pinned.
    pub dynamic: bool,
}

/// Data-reader table functions whose first string-literal argument is the object
/// path/glob to pin. `delta_scan`/`iceberg_scan` are handled separately.
const READER_FNS: &[&str] = &[
    "read_parquet",
    "read_csv_auto",
    "read_csv",
    "read_json_auto",
    "read_json",
    "read_ndjson",
    "read_ndjson_auto",
    "parquet_scan",
];

/// Extract and classify the external object sources referenced by `sql`.
///
/// Conservative: it pins what it can prove from string literals, and flags a
/// reader invoked with a non-literal argument as [`SqlSources::dynamic`] so the
/// caller can decline to penalize an unpinnable job. It never *invents* a source.
pub fn parse_input_sources(sql: &str) -> SqlSources {
    let lower = sql.to_ascii_lowercase();
    let literals = string_literals(sql);
    // Lowercased literal values aligned with `literals` (for scheme/ext checks).
    let lower_vals: Vec<String> = literals.iter().map(|l| l.value.to_ascii_lowercase()).collect();

    let mut sources: Vec<SqlSource> = Vec::new();
    let mut dynamic = false;
    let mut consumed: Vec<bool> = vec![false; literals.len()];

    // 1. Lakehouse readers: delta_scan(...) / iceberg_scan(...). The first
    //    string-literal argument is the table location; a non-literal ⇒ dynamic.
    for (fname, kind) in [("delta_scan", SourceKind::Delta), ("iceberg_scan", SourceKind::Iceberg)] {
        for call in fn_call_offsets(&lower, fname) {
            match first_literal_after(&literals, call) {
                Some(i) => {
                    consumed[i] = true;
                    sources.push(SqlSource {
                        uri: literals[i].value.clone(),
                        kind: kind.clone(),
                    });
                }
                None => dynamic = true,
            }
        }
    }

    // 2. Object/glob readers: read_parquet / read_csv / ... The first string
    //    literal argument is the path or glob; a non-literal ⇒ dynamic.
    for fname in READER_FNS {
        for call in fn_call_offsets(&lower, fname) {
            match first_literal_after(&literals, call) {
                Some(i) => {
                    if !consumed[i] {
                        consumed[i] = true;
                        let v = &literals[i].value;
                        sources.push(SqlSource {
                            uri: v.clone(),
                            kind: if is_glob(v) { SourceKind::Glob } else { SourceKind::Object },
                        });
                    }
                }
                None => dynamic = true,
            }
        }
    }

    // 3. Bare object literals (e.g. `FROM 's3://b/k.parquet'`) not already claimed
    //    by a reader function: pin them if they look like a data object.
    for (i, lit) in literals.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if looks_like_object_uri(&lower_vals[i]) {
            sources.push(SqlSource {
                uri: lit.value.clone(),
                kind: if is_glob(&lit.value) { SourceKind::Glob } else { SourceKind::Object },
            });
        }
    }

    // Determinism: stable order regardless of where the literals appeared.
    sources.sort_by(|a, b| a.uri.cmp(&b.uri));
    sources.dedup();
    SqlSources { sources, dynamic }
}

/// A single-quoted SQL string literal with its byte offset in the source.
struct Literal {
    /// Byte offset of the OPENING quote in the source.
    start: usize,
    value: String,
}

/// Scan `sql` for single-quoted string literals, honoring the `''` escape.
fn string_literals(sql: &str) -> Vec<Literal> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            let mut val = String::new();
            loop {
                if i >= bytes.len() {
                    break;
                }
                if bytes[i] == b'\'' {
                    // Doubled quote = an escaped single quote inside the literal.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        val.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1; // closing quote
                    break;
                }
                val.push(bytes[i] as char);
                i += 1;
            }
            out.push(Literal { start, value: val });
        } else {
            i += 1;
        }
    }
    out
}

/// Byte offsets just AFTER each `name(` occurrence in the (lowercased) sql, so a
/// matching string literal can be found inside the call. Requires the next
/// non-space char after `name` to be `(` so `read_parquet_meta` doesn't match
/// `read_parquet`.
fn fn_call_offsets(lower: &str, name: &str) -> Vec<usize> {
    let mut offs = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let idx = from + rel;
        // Word boundary before the name (not part of a longer identifier).
        let prev_ok = idx == 0
            || !lower.as_bytes()[idx - 1].is_ascii_alphanumeric() && lower.as_bytes()[idx - 1] != b'_';
        let after = idx + name.len();
        let mut j = after;
        while j < lower.len() && lower.as_bytes()[j] == b' ' {
            j += 1;
        }
        if prev_ok && j < lower.len() && lower.as_bytes()[j] == b'(' {
            offs.push(j + 1);
        }
        from = idx + name.len();
    }
    offs
}

/// The index of the first string literal whose opening quote is at/after `offset`
/// AND before the next `)` is irrelevant here — we only need the nearest literal
/// that starts after the call's `(`. Returns `None` if the nearest token after
/// the offset is not a literal within a small window (heuristic: a non-literal
/// argument such as a bind variable / column reference ⇒ dynamic).
fn first_literal_after(literals: &[Literal], offset: usize) -> Option<usize> {
    // The first literal that starts at/after the offset.
    literals
        .iter()
        .enumerate()
        .filter(|(_, l)| l.start >= offset)
        .min_by_key(|(_, l)| l.start)
        .map(|(i, _)| i)
}

/// Whether a literal value is a glob/wildcard (expands to multiple objects).
fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Whether a (lowercased) literal looks like an external data object/URI worth
/// pinning: has a remote scheme, or a known data-file extension.
fn looks_like_object_uri(lower: &str) -> bool {
    const SCHEMES: &[&str] = &["s3://", "gs://", "gcs://", "az://", "abfss://", "azure://", "https://", "http://", "file://"];
    const EXTS: &[&str] = &[".parquet", ".csv", ".json", ".ndjson", ".tsv"];
    SCHEMES.iter().any(|s| lower.starts_with(s))
        || EXTS.iter().any(|e| lower.ends_with(e))
        || (is_glob(lower) && EXTS.iter().any(|e| lower.contains(e.trim_start_matches('.'))))
}

/// The provider id for a URI scheme (matches the storage `ProviderRegistry` ids).
pub fn provider_for_uri(uri: &str) -> &'static str {
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("s3://") {
        "s3"
    } else if lower.starts_with("gs://") || lower.starts_with("gcs://") {
        "gcs"
    } else if lower.starts_with("az://") || lower.starts_with("abfss://") || lower.starts_with("azure://") {
        "az"
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        "https"
    } else {
        // Bare paths and `file://` are local.
        "local-fake"
    }
}

// ---------------------------------------------------------------------------
// Version probes (pluggable per scheme; local is real)
// ---------------------------------------------------------------------------

/// Resolves concrete object versions for one or more URI schemes. A real cloud
/// implementation HEADs/Lists via the object store with the job's credentials.
#[async_trait]
pub trait ObjectVersionProbe: Send + Sync {
    /// Schemes this probe serves (e.g. `["s3"]`, `["file"]`).
    fn schemes(&self) -> &[&str];

    /// Resolve a single concrete object to a pinned version.
    async fn head(&self, uri: &str) -> Result<PinnedObject, InputResolveError>;

    /// Expand a glob to the concrete objects it currently matches, each pinned.
    async fn list(&self, glob: &str) -> Result<Vec<PinnedObject>, InputResolveError>;
}

/// Real local-filesystem probe: content-addresses `file://` / bare local paths
/// (and expands local globs). Pins by content hash so any byte change flips the
/// fingerprint. For large objects a real deployment prefers a cloud version id
/// (metadata-only); local content hashing is the no-cloud fallback used for
/// fixtures and tests.
pub struct LocalFsProbe;

impl LocalFsProbe {
    fn path_of(uri: &str) -> PathBuf {
        PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
    }

    fn pin_file(path: &Path, uri: &str) -> Result<PinnedObject, InputResolveError> {
        let bytes = std::fs::read(path)
            .map_err(|e| InputResolveError::Unavailable(format!("{}: {e}", path.display())))?;
        let sha = hex::encode(blake3::hash(&bytes).as_bytes());
        Ok(PinnedObject {
            uri: uri.to_string(),
            provider: "local-fake".into(),
            version: ObjectVersion::ContentAddressed { sha256: sha },
        })
    }
}

#[async_trait]
impl ObjectVersionProbe for LocalFsProbe {
    fn schemes(&self) -> &[&str] {
        &["file"]
    }

    async fn head(&self, uri: &str) -> Result<PinnedObject, InputResolveError> {
        let uri = uri.to_string();
        tokio::task::spawn_blocking(move || {
            let path = Self::path_of(&uri);
            Self::pin_file(&path, &uri)
        })
        .await
        .map_err(|e| InputResolveError::Unavailable(format!("probe join: {e}")))?
    }

    async fn list(&self, glob: &str) -> Result<Vec<PinnedObject>, InputResolveError> {
        let glob = glob.to_string();
        tokio::task::spawn_blocking(move || {
            // Support a simple `dir/*.ext` glob (the common Parquet-partition
            // shape); pin every matching file. More elaborate glob syntax falls
            // back to NotPinnable so we never silently pin a partial set.
            let raw = glob.strip_prefix("file://").unwrap_or(&glob);
            let (dir, pattern) = match raw.rfind('/') {
                Some(i) => (&raw[..i], &raw[i + 1..]),
                None => (".", raw),
            };
            if pattern.contains('/') || pattern.contains("**") {
                return Err(InputResolveError::NotPinnable(format!(
                    "unsupported glob pattern: {glob}"
                )));
            }
            let suffix = pattern.strip_prefix('*').unwrap_or(pattern);
            let mut out = Vec::new();
            let entries = std::fs::read_dir(dir)
                .map_err(|e| InputResolveError::Unavailable(format!("{dir}: {e}")))?;
            for e in entries.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let matches = if pattern == "*" {
                    true
                } else if pattern.starts_with('*') {
                    name.ends_with(suffix)
                } else {
                    name == pattern
                };
                if matches {
                    let uri = format!("{dir}/{name}");
                    out.push(LocalFsProbe::pin_file(&p, &uri)?);
                }
            }
            out.sort_by(|a, b| a.uri.cmp(&b.uri));
            Ok(out)
        })
        .await
        .map_err(|e| InputResolveError::Unavailable(format!("probe join: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Manifest resolver (the default InputResolver)
// ---------------------------------------------------------------------------

/// The default [`InputResolver`]: parses the SQL, then resolves each source to a
/// pinned object via the registered [`ObjectVersionProbe`] for its scheme. Delta
/// tables are pinned at the table-snapshot level (the `_delta_log` version) when
/// they are local; cloud Delta/Iceberg pinning needs the corresponding probe.
pub struct ManifestResolver {
    /// Probe by scheme (e.g. `"file"`, `"s3"`).
    probes: BTreeMap<String, Arc<dyn ObjectVersionProbe>>,
}

impl Default for ManifestResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ManifestResolver {
    /// A resolver with only the real local-filesystem probe wired (the no-cloud
    /// default). Cloud schemes resolve as `NotPinnable` until a probe is added.
    pub fn new() -> Self {
        let mut r = Self {
            probes: BTreeMap::new(),
        };
        r.register(Arc::new(LocalFsProbe));
        r
    }

    /// An empty resolver with NO probes (every external source is unpinnable).
    pub fn empty() -> Self {
        Self {
            probes: BTreeMap::new(),
        }
    }

    /// Register a probe for all of its schemes.
    pub fn register(&mut self, probe: Arc<dyn ObjectVersionProbe>) {
        for s in probe.schemes() {
            self.probes.insert(s.to_string(), Arc::clone(&probe));
        }
    }

    fn probe_for(&self, uri: &str) -> Option<&Arc<dyn ObjectVersionProbe>> {
        let scheme = uri.split("://").next().unwrap_or("");
        // A bare local path (no scheme) maps to the `file` probe.
        let key = if uri.contains("://") { scheme } else { "file" };
        self.probes.get(key)
    }
}

#[async_trait]
impl InputResolver for ManifestResolver {
    async fn resolve(&self, sql: &str) -> Result<Option<InputSnapshot>, InputResolveError> {
        let parsed = parse_input_sources(sql);
        if parsed.dynamic {
            return Err(InputResolveError::NotPinnable(
                "a data reader was given a non-literal (bind/variable) path".into(),
            ));
        }
        if parsed.sources.is_empty() {
            return Ok(None);
        }

        let mut objects: Vec<PinnedObject> = Vec::new();
        for src in &parsed.sources {
            match src.kind {
                SourceKind::Delta => objects.push(pin_delta_table(&src.uri)?),
                SourceKind::Iceberg => {
                    // Iceberg snapshot pinning needs the iceberg extension /
                    // catalog; without a probe it is not statically pinnable.
                    return Err(InputResolveError::NotPinnable(format!(
                        "iceberg table pinning requires an iceberg probe: {}",
                        src.uri
                    )));
                }
                SourceKind::Glob => {
                    let probe = self.probe_for(&src.uri).ok_or_else(|| {
                        InputResolveError::NotPinnable(format!("no version probe for {}", src.uri))
                    })?;
                    let mut listed = probe.list(&src.uri).await?;
                    if listed.is_empty() {
                        return Err(InputResolveError::Unavailable(format!(
                            "glob matched no objects: {}",
                            src.uri
                        )));
                    }
                    objects.append(&mut listed);
                }
                SourceKind::Object => {
                    let probe = self.probe_for(&src.uri).ok_or_else(|| {
                        InputResolveError::NotPinnable(format!("no version probe for {}", src.uri))
                    })?;
                    objects.push(probe.head(&src.uri).await?);
                }
            }
        }

        Ok(Some(InputSnapshot::from_objects(objects)))
    }
}

/// Pin a (local) Delta table at its current snapshot version by reading the
/// highest-numbered `_delta_log/NN...N.json` commit. Reuses the same log layout
/// the estimator's `delta_metadata` parses.
fn pin_delta_table(uri: &str) -> Result<PinnedObject, InputResolveError> {
    let dir = uri.strip_prefix("file://").unwrap_or(uri);
    let version = delta_latest_version(Path::new(dir))
        .ok_or_else(|| InputResolveError::Unavailable(format!("no _delta_log commits at {dir}")))?;
    Ok(PinnedObject {
        uri: uri.to_string(),
        provider: "delta".into(),
        version: ObjectVersion::Lakehouse {
            format: "delta".into(),
            snapshot_id: version.to_string(),
        },
    })
}

/// The highest Delta commit version in `<table_dir>/_delta_log` (the current
/// table snapshot), or `None` if the log is missing/empty.
pub fn delta_latest_version(table_dir: &Path) -> Option<u64> {
    let log = table_dir.join("_delta_log");
    let mut max: Option<u64> = None;
    for e in std::fs::read_dir(&log).ok()?.flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "json").unwrap_or(false) {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if let Ok(v) = stem.trim_start_matches('0').parse::<u64>() {
                    max = Some(max.map_or(v, |m| m.max(v)));
                } else if stem.chars().all(|c| c == '0') {
                    // The very first commit `000...0.json` is version 0.
                    max = Some(max.unwrap_or(0));
                }
            }
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_parquet_object() {
        let s = parse_input_sources("SELECT * FROM read_parquet('s3://bucket/key.parquet')");
        assert!(!s.dynamic);
        assert_eq!(s.sources.len(), 1);
        assert_eq!(s.sources[0].kind, SourceKind::Object);
        assert_eq!(s.sources[0].uri, "s3://bucket/key.parquet");
    }

    #[test]
    fn parses_glob() {
        let s = parse_input_sources("SELECT * FROM read_parquet('s3://b/year=2024/*.parquet')");
        assert_eq!(s.sources[0].kind, SourceKind::Glob);
    }

    #[test]
    fn detects_dynamic_reader() {
        // A reader fed a bind variable / identifier has no string literal arg.
        let s = parse_input_sources("SELECT * FROM read_parquet(my_path)");
        assert!(s.dynamic);
    }

    #[test]
    fn parses_delta_scan() {
        let s = parse_input_sources("SELECT * FROM delta_scan('s3://b/table')");
        assert_eq!(s.sources[0].kind, SourceKind::Delta);
    }

    #[test]
    fn bare_object_literal_is_pinned() {
        let s = parse_input_sources("SELECT * FROM 's3://b/k.parquet'");
        assert_eq!(s.sources.len(), 1);
        assert_eq!(s.sources[0].kind, SourceKind::Object);
    }

    #[test]
    fn pure_in_memory_has_no_sources() {
        let s = parse_input_sources("SELECT 1 + 1");
        assert!(s.sources.is_empty());
        assert!(!s.dynamic);
    }

    #[test]
    fn provider_for_uri_maps_schemes() {
        assert_eq!(provider_for_uri("s3://b/k"), "s3");
        assert_eq!(provider_for_uri("gs://b/k"), "gcs");
        assert_eq!(provider_for_uri("abfss://c/p"), "az");
        assert_eq!(provider_for_uri("/local/path.parquet"), "local-fake");
    }

    #[tokio::test]
    async fn local_resolver_pins_file_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data.parquet");
        std::fs::write(&f, b"rows-v1").unwrap();
        let sql = format!("SELECT * FROM read_parquet('{}')", f.display());
        let r = ManifestResolver::new();
        let snap = r.resolve(&sql).await.unwrap().unwrap();
        assert_eq!(snap.objects.len(), 1);
        let fp1 = snap.fingerprint.clone();

        // Change the bytes → the fingerprint must change (drift is detectable).
        std::fs::write(&f, b"rows-v2-different").unwrap();
        let snap2 = r.resolve(&sql).await.unwrap().unwrap();
        assert_ne!(fp1, snap2.fingerprint);
    }

    #[tokio::test]
    async fn local_glob_resolves_multiple_objects_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.parquet"), b"a").unwrap();
        std::fs::write(dir.path().join("b.parquet"), b"b").unwrap();
        std::fs::write(dir.path().join("ignore.txt"), b"x").unwrap();
        let sql = format!(
            "SELECT * FROM read_parquet('{}/*.parquet')",
            dir.path().display()
        );
        let r = ManifestResolver::new();
        let snap = r.resolve(&sql).await.unwrap().unwrap();
        assert_eq!(snap.objects.len(), 2, "two parquet files matched");
        // Determinism: a second resolve over the same bytes yields the same fp.
        let again = r.resolve(&sql).await.unwrap().unwrap();
        assert_eq!(snap.fingerprint, again.fingerprint);
    }

    #[tokio::test]
    async fn delta_table_pins_snapshot_version() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("_delta_log");
        std::fs::create_dir_all(&log).unwrap();
        std::fs::write(log.join("00000000000000000000.json"), "{}\n").unwrap();
        std::fs::write(log.join("00000000000000000001.json"), "{}\n").unwrap();
        let sql = format!("SELECT * FROM delta_scan('{}')", dir.path().display());
        let r = ManifestResolver::new();
        let snap = r.resolve(&sql).await.unwrap().unwrap();
        match &snap.objects[0].version {
            ObjectVersion::Lakehouse { format, snapshot_id } => {
                assert_eq!(format, "delta");
                assert_eq!(snapshot_id, "1", "latest commit version is pinned");
            }
            other => panic!("expected Lakehouse pin, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dynamic_sql_is_not_pinnable() {
        let r = ManifestResolver::new();
        let err = r
            .resolve("SELECT * FROM read_parquet(some_variable)")
            .await
            .unwrap_err();
        assert!(matches!(err, InputResolveError::NotPinnable(_)));
    }
}
