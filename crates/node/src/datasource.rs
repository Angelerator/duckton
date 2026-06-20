//! Pluggable data-source layer: formats + storage providers + the secure-read
//! setup plan for the locked-down DuckDB engine (architecture §4, §9.2, §9.4).
//!
//! This module is the extension point that lets new **formats** (Parquet, CSV,
//! JSON, Delta, Iceberg, …) and new **storage providers** (AWS S3, Azure ADLS,
//! Google Cloud Storage, generic HTTPS, …) be added without rewrites, mirroring
//! the project's existing pluggable-trait pattern (`QueryEngine`, `Discovery`,
//! `TrustStore`, `StorageCredentialProvider`).
//!
//! ## What lives here
//! * [`DataFormat`] — a format + the DuckDB extensions it needs.
//! * [`StorageProvider`] — a trait that turns a per-job [`ScopedCredential`]
//!   into a DuckDB `CREATE SECRET` statement (S3 STS session creds, Azure
//!   user-delegation SAS, GCS HMAC/downscoped tokens). Implementations:
//!   [`S3Provider`], [`AzureProvider`], [`GcsProvider`], [`HttpsProvider`],
//!   [`LocalFileProvider`].
//! * [`ProviderRegistry`] — provider lookup by id, built from config.
//! * [`StorageSetup`] — the *engine-init* decision (which extensions to
//!   pre-load, whether to enable network egress, which local dirs are allowed),
//!   derived once from [`p2p_config::StorageConfig`] — never influenced by the
//!   untrusted query text.
//!
//! ## Honest scope
//! Secret-SQL generation is real and unit-tested. Actually *installing* an S3 /
//! Azure / GCS secret needs the `httpfs` / `azure` extensions loaded, and
//! reading real cloud objects needs live cloud credentials — see the module
//! tests and `docs/ARCHITECTURE.md` for what is tested vs. mocked vs. requires
//! real cloud.

use std::collections::BTreeMap;
use std::sync::Arc;

use p2p_proto::ScopedCredential;
use p2p_trust::{seal_to, SealedBlob, SealingKeypair};

/// Errors from the data-source layer.
#[derive(Debug, thiserror::Error)]
pub enum DataSourceError {
    #[error("unknown storage provider: {0}")]
    UnknownProvider(String),
    #[error("credential is missing required field `{0}` for provider `{1}`")]
    MissingField(String, String),
    #[error("malformed credential token: {0}")]
    MalformedToken(String),
    #[error("unsupported data format: {0}")]
    UnsupportedFormat(String),
    /// A sealed credential token was supplied but this worker has no sealing
    /// keypair to open it (or `from_scoped` was used instead of the unsealing
    /// path). The plaintext is never recoverable without the worker key.
    #[error("sealed credential supplied but no worker sealing key is available to open it")]
    SealingKeyUnavailable,
    /// The sealed credential could not be opened with the worker's key (wrong
    /// recipient, tampered ciphertext, or corrupt blob). Deliberately carries
    /// no key material in its message.
    #[error("failed to open sealed credential (wrong recipient key or tampered ciphertext)")]
    SealOpenFailed,
    /// A provider-specific `extra` secret key is not a safe SQL identifier. The
    /// key is interpolated *unquoted* into a `CREATE SECRET (...)` statement, so
    /// it must match `^[A-Z_][A-Z0-9_]*$` (after uppercasing) — otherwise it
    /// could break out of the option list and inject SQL.
    #[error("invalid secret option key `{0}` (must match [A-Z_][A-Z0-9_]*)")]
    InvalidSecretKey(String),
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

/// A data format a worker can read. Extensible: unknown strings map to
/// [`DataFormat::Other`] so new formats can be enabled by config alone once the
/// matching DuckDB extension is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataFormat {
    Csv,
    Json,
    Parquet,
    Delta,
    Iceberg,
    Other(String),
}

impl DataFormat {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "csv" => DataFormat::Csv,
            "json" | "ndjson" => DataFormat::Json,
            "parquet" => DataFormat::Parquet,
            "delta" | "deltalake" | "delta_lake" => DataFormat::Delta,
            "iceberg" => DataFormat::Iceberg,
            other => DataFormat::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            DataFormat::Csv => "csv",
            DataFormat::Json => "json",
            DataFormat::Parquet => "parquet",
            DataFormat::Delta => "delta",
            DataFormat::Iceberg => "iceberg",
            DataFormat::Other(s) => s.as_str(),
        }
    }

    /// DuckDB extensions required to read this format. CSV is core (no
    /// extension); Parquet/JSON are statically linked in our bundled build;
    /// Delta/Iceberg are separate extensions that must be pre-loaded.
    pub fn required_extensions(&self) -> Vec<&'static str> {
        match self {
            DataFormat::Csv => vec![],
            DataFormat::Json => vec!["json"],
            DataFormat::Parquet => vec!["parquet"],
            // delta reads underlying parquet, so it needs both.
            DataFormat::Delta => vec!["delta", "parquet"],
            DataFormat::Iceberg => vec!["iceberg", "parquet"],
            DataFormat::Other(_) => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Credential material
// ---------------------------------------------------------------------------

/// Structured cloud credential material carried (JSON-encoded) inside the
/// opaque [`ScopedCredential::token`]. Keeping it inside the existing opaque
/// token avoids a wire-protocol change while letting providers build typed
/// `CREATE SECRET` statements. All fields are short-lived per the architecture
/// (STS session token / SAS / downscoped token) — never long-lived keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CloudCredential {
    /// AWS/GCS access key id (or HMAC key id for GCS interop).
    pub key_id: Option<String>,
    /// AWS/GCS secret key (or HMAC secret).
    pub secret: Option<String>,
    /// Short-lived STS session token (AWS) — the heart of "scoped & expiring".
    pub session_token: Option<String>,
    /// Azure connection string or (preferably) a user-delegation SAS token.
    pub connection_string: Option<String>,
    /// Region override (cloud).
    pub region: Option<String>,
    /// Custom endpoint (S3-compatible / MinIO / private link), e.g.
    /// `minio.local:9000`. Maps to the DuckDB s3 secret `ENDPOINT`.
    pub endpoint: Option<String>,
    /// S3 URL addressing style: `"path"` (MinIO / most self-hosted
    /// S3-compatible stores) or `"vhost"` (AWS default). Maps to `URL_STYLE`.
    pub url_style: Option<String>,
    /// Whether to use TLS for the S3 endpoint. Maps to `USE_SSL`. MinIO dev
    /// deployments frequently run plain HTTP (`Some(false)`).
    pub use_ssl: Option<bool>,
    /// Any provider-specific extra `CREATE SECRET` key/values.
    pub extra: BTreeMap<String, String>,
}

/// Sentinel prefix marking a **sealed** (encrypted) credential token. The
/// remainder is `hex(JSON(SealedBlob))` whose plaintext is the JSON
/// [`CloudCredential`], sealed (X25519 + ChaCha20-Poly1305) to a worker's
/// sealing public key (architecture §9.2/§9.3). The access key / secret are
/// therefore never carried, stored or logged in plaintext — they are decrypted
/// only just-in-time inside the worker at engine setup.
pub const SEALED_TOKEN_PREFIX: &str = "sealed:v1:";

impl CloudCredential {
    /// Decode the structured material from a plaintext [`ScopedCredential`]
    /// token.
    ///
    /// The local-fake provider issues a plain hex token (no JSON); in that case
    /// we return an empty credential, which is fine because the local-fake path
    /// does not create a cloud secret. A **sealed** token is rejected here so a
    /// misconfigured worker (no sealing key) fails loudly rather than silently
    /// minting an empty secret — use [`CloudCredential::unseal`] for those.
    pub fn from_scoped(cred: &ScopedCredential) -> Result<Self, DataSourceError> {
        let t = cred.token.trim();
        if Self::is_sealed_token(t) {
            return Err(DataSourceError::SealingKeyUnavailable);
        }
        if t.is_empty() || !t.starts_with('{') {
            return Ok(CloudCredential::default());
        }
        serde_json::from_str(t).map_err(|e| DataSourceError::MalformedToken(e.to_string()))
    }

    /// Encode to the opaque (plaintext JSON) token string (used by fake
    /// providers / tests / after just-in-time unsealing).
    pub fn to_token(&self) -> String {
        serde_json::to_string(self).expect("CloudCredential serializes")
    }

    /// True if `token` is a sealed credential token.
    pub fn is_sealed_token(token: &str) -> bool {
        token.trim_start().starts_with(SEALED_TOKEN_PREFIX)
    }

    /// **Requester side.** Seal this credential to `recipient_pub` — the
    /// worker's X25519 sealing public key, learned from its attestation quote
    /// (`Enclave::attest` binds it) or signed capability record — and return
    /// the opaque token string to carry in a [`ScopedCredential`]. The
    /// plaintext key material never leaves the requester unencrypted.
    pub fn seal_token(&self, recipient_pub: &[u8; 32]) -> String {
        let json = serde_json::to_vec(self).expect("CloudCredential serializes");
        let blob = seal_to(recipient_pub, &json);
        format!("{SEALED_TOKEN_PREFIX}{}", blob.to_hex())
    }

    /// **Worker side.** Open a sealed token just-in-time with the worker's
    /// sealing `keypair`. Returns [`DataSourceError::SealOpenFailed`] for a
    /// wrong recipient or tampered ciphertext (no key material is leaked).
    pub fn unseal(token: &str, keypair: &SealingKeypair) -> Result<Self, DataSourceError> {
        let hex = token
            .trim_start()
            .strip_prefix(SEALED_TOKEN_PREFIX)
            .ok_or_else(|| DataSourceError::MalformedToken("not a sealed token".into()))?;
        let blob = SealedBlob::from_hex(hex).ok_or(DataSourceError::SealOpenFailed)?;
        let plaintext = keypair.open(&blob).ok_or(DataSourceError::SealOpenFailed)?;
        serde_json::from_slice(&plaintext)
            .map_err(|e| DataSourceError::MalformedToken(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Provider trait + implementations
// ---------------------------------------------------------------------------

/// Resolved per-provider options (endpoint/region/url-style overrides from
/// `[storage.provider_options.<id>]`).
#[derive(Debug, Clone, Default)]
pub struct ProviderOptions {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// S3 URL addressing style (`"path"` for MinIO, `"vhost"` for AWS).
    pub url_style: Option<String>,
    /// Whether the S3 endpoint uses TLS (`USE_SSL`).
    pub use_ssl: Option<bool>,
    pub extra: BTreeMap<String, String>,
}

/// A storage provider: knows its URL schemes, the DuckDB extensions it needs,
/// and how to mint a `CREATE SECRET` statement for a per-job scoped credential.
///
/// Pluggable: add a provider for a new cloud by implementing this trait and
/// registering it; no engine changes required.
pub trait StorageProvider: Send + Sync {
    /// Stable id matching [`ScopedCredential::provider`] and config.
    fn provider_id(&self) -> &str;

    /// URL schemes this provider serves (e.g. `["s3"]`, `["az","abfss"]`).
    fn url_schemes(&self) -> &[&str];

    /// DuckDB extensions this provider needs loaded to function.
    fn required_extensions(&self) -> &[&str];

    /// Build a `CREATE SECRET` statement that scopes the credential to its
    /// read-only prefix. `name` is the secret's (unique-per-connection) name.
    ///
    /// Returns `Ok(None)` for providers that need no secret (local, public
    /// HTTPS). Values are SQL-escaped to neutralize quote injection.
    fn create_secret_sql(
        &self,
        name: &str,
        cred: &ScopedCredential,
        opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError>;
}

/// Single-quote escape for embedding values in DuckDB SQL string literals.
fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// RFC-3986 percent-encoding as required by AWS SigV4 (unreserved set
/// `A-Za-z0-9-_.~` pass through; `/` is preserved in a path unless
/// `encode_slash`). Used to build presigned S3 URLs and to encode the object
/// path in the no-cloud fake presigner.
pub fn aws_uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// **Worker side, presigned credential mode.** Rewrite the job SQL so each pinned
/// input object reference (a single-quoted `'<uri>'` literal) is replaced by its
/// presigned HTTPS `'<url>'`, letting the worker read it with plain HTTPS and NO
/// `CREATE SECRET`. Only the exact, single-quoted object literal is replaced, so
/// unrelated text is never touched. Longer URIs are substituted first so a URI
/// that is a prefix of another cannot shadow it.
pub fn rewrite_signed_urls(sql: &str, signed: &[p2p_proto::SignedInput]) -> String {
    let mut pairs: Vec<&p2p_proto::SignedInput> = signed.iter().collect();
    pairs.sort_by(|a, b| b.uri.len().cmp(&a.uri.len()));
    let mut out = sql.to_string();
    for s in pairs {
        if s.uri.is_empty() {
            continue;
        }
        let from = sql_lit(&s.uri);
        let to = sql_lit(&s.url);
        out = out.replace(&from, &to);
    }
    out
}

/// Validate and normalize a provider-specific `extra` secret OPTION KEY for safe,
/// *unquoted* interpolation into a `CREATE SECRET (...)` option list. Unlike the
/// option *values* (escaped via [`sql_lit`]), the key is an SQL identifier and is
/// not quoted, so an unvalidated key (e.g. `"x) AS y; ATTACH ..."`) could break
/// out of the statement. We uppercase (DuckDB option keys are case-insensitive)
/// and require `^[A-Z_][A-Z0-9_]*$`.
fn secret_key_ident(key: &str) -> Result<String, DataSourceError> {
    let upper = key.to_uppercase();
    let mut chars = upper.chars();
    let valid = match chars.next() {
        Some(c) if c == '_' || c.is_ascii_uppercase() => {
            chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit())
        }
        _ => false,
    };
    if !valid {
        return Err(DataSourceError::InvalidSecretKey(key.to_string()));
    }
    Ok(upper)
}

/// Normalize a credential prefix into a fully-qualified, slash-terminated SCOPE
/// for the given scheme. DuckDB matches the longest scope prefix, and requires
/// a trailing slash for directory scopes.
fn scope_url(scheme: &str, prefix: &str) -> String {
    let p = prefix.trim();
    let mut url = if p.contains("://") {
        p.to_string()
    } else {
        format!("{scheme}://{}", p.trim_start_matches('/'))
    };
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

/// AWS S3 (and S3-compatible) provider — uses `httpfs` + (optionally) `aws`.
/// Credentials are STS session tokens (`SESSION_TOKEN`), read-only & short-TTL.
pub struct S3Provider {
    schemes: Vec<&'static str>,
    extensions: Vec<&'static str>,
}

impl Default for S3Provider {
    fn default() -> Self {
        Self {
            schemes: vec!["s3"],
            extensions: vec!["httpfs"],
        }
    }
}

impl StorageProvider for S3Provider {
    fn provider_id(&self) -> &str {
        "s3"
    }
    fn url_schemes(&self) -> &[&str] {
        &self.schemes
    }
    fn required_extensions(&self) -> &[&str] {
        &self.extensions
    }
    fn create_secret_sql(
        &self,
        name: &str,
        cred: &ScopedCredential,
        opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError> {
        let c = CloudCredential::from_scoped(cred)?;
        let key_id = c
            .key_id
            .ok_or_else(|| DataSourceError::MissingField("key_id".into(), "s3".into()))?;
        let secret = c
            .secret
            .ok_or_else(|| DataSourceError::MissingField("secret".into(), "s3".into()))?;
        // Per-call credential material wins over per-provider config options
        // (defaults -> TOML -> env -> per-call layering, resolved here).
        let region = c.region.or_else(|| opts.region.clone());
        let endpoint = c.endpoint.or_else(|| opts.endpoint.clone());
        let url_style = c.url_style.or_else(|| opts.url_style.clone());
        let use_ssl = c.use_ssl.or(opts.use_ssl);

        let mut parts = vec![
            "TYPE s3".to_string(),
            format!("KEY_ID {}", sql_lit(&key_id)),
            format!("SECRET {}", sql_lit(&secret)),
        ];
        if let Some(tok) = c.session_token {
            parts.push(format!("SESSION_TOKEN {}", sql_lit(&tok)));
        }
        if let Some(r) = region {
            parts.push(format!("REGION {}", sql_lit(&r)));
        }
        if let Some(e) = endpoint {
            parts.push(format!("ENDPOINT {}", sql_lit(&e)));
        }
        if let Some(us) = url_style {
            // S3-compatible / MinIO uses path-style addressing.
            parts.push(format!("URL_STYLE {}", sql_lit(&us)));
        }
        if let Some(ssl) = use_ssl {
            // USE_SSL is a DuckDB boolean literal (unquoted), not a string.
            parts.push(format!("USE_SSL {}", if ssl { "true" } else { "false" }));
        }
        for (k, v) in &c.extra {
            parts.push(format!("{} {}", secret_key_ident(k)?, sql_lit(v)));
        }
        parts.push(format!("SCOPE {}", sql_lit(&scope_url("s3", &cred.prefix))));
        Ok(Some(format!(
            "CREATE OR REPLACE SECRET {name} ({});",
            parts.join(", ")
        )))
    }
}

/// Azure ADLS / Blob provider (`abfss://` / `az://`) — uses the `azure`
/// extension. Prefers a path-scoped, read-only, short-expiry user-delegation
/// SAS passed as the connection string.
pub struct AzureProvider {
    schemes: Vec<&'static str>,
    extensions: Vec<&'static str>,
}

impl Default for AzureProvider {
    fn default() -> Self {
        Self {
            schemes: vec!["az", "abfss", "azure"],
            extensions: vec!["azure"],
        }
    }
}

impl StorageProvider for AzureProvider {
    fn provider_id(&self) -> &str {
        "az"
    }
    fn url_schemes(&self) -> &[&str] {
        &self.schemes
    }
    fn required_extensions(&self) -> &[&str] {
        &self.extensions
    }
    fn create_secret_sql(
        &self,
        name: &str,
        cred: &ScopedCredential,
        _opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError> {
        let c = CloudCredential::from_scoped(cred)?;
        let conn = c.connection_string.ok_or_else(|| {
            DataSourceError::MissingField("connection_string".into(), "az".into())
        })?;
        let mut parts = vec![
            "TYPE azure".to_string(),
            format!("CONNECTION_STRING {}", sql_lit(&conn)),
        ];
        for (k, v) in &c.extra {
            parts.push(format!("{} {}", secret_key_ident(k)?, sql_lit(v)));
        }
        parts.push(format!(
            "SCOPE {}",
            sql_lit(&scope_url("azure", &cred.prefix))
        ));
        Ok(Some(format!(
            "CREATE OR REPLACE SECRET {name} ({});",
            parts.join(", ")
        )))
    }
}

/// Google Cloud Storage provider (`gcs://` / `gs://`). DuckDB accesses GCS via
/// the S3-compatible API using the dedicated `GCS` secret type with HMAC keys,
/// or short-lived downscoped tokens delivered as the secret/session token.
pub struct GcsProvider {
    schemes: Vec<&'static str>,
    extensions: Vec<&'static str>,
}

impl Default for GcsProvider {
    fn default() -> Self {
        Self {
            schemes: vec!["gcs", "gs"],
            extensions: vec!["httpfs"],
        }
    }
}

impl StorageProvider for GcsProvider {
    fn provider_id(&self) -> &str {
        "gcs"
    }
    fn url_schemes(&self) -> &[&str] {
        &self.schemes
    }
    fn required_extensions(&self) -> &[&str] {
        &self.extensions
    }
    fn create_secret_sql(
        &self,
        name: &str,
        cred: &ScopedCredential,
        opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError> {
        let c = CloudCredential::from_scoped(cred)?;
        let key_id = c
            .key_id
            .ok_or_else(|| DataSourceError::MissingField("key_id".into(), "gcs".into()))?;
        let secret = c
            .secret
            .ok_or_else(|| DataSourceError::MissingField("secret".into(), "gcs".into()))?;
        let mut parts = vec![
            "TYPE gcs".to_string(),
            format!("KEY_ID {}", sql_lit(&key_id)),
            format!("SECRET {}", sql_lit(&secret)),
        ];
        if let Some(tok) = c.session_token {
            parts.push(format!("SESSION_TOKEN {}", sql_lit(&tok)));
        }
        if let Some(e) = c.endpoint.or_else(|| opts.endpoint.clone()) {
            parts.push(format!("ENDPOINT {}", sql_lit(&e)));
        }
        for (k, v) in &c.extra {
            parts.push(format!("{} {}", secret_key_ident(k)?, sql_lit(v)));
        }
        parts.push(format!(
            "SCOPE {}",
            sql_lit(&scope_url("gcs", &cred.prefix))
        ));
        Ok(Some(format!(
            "CREATE OR REPLACE SECRET {name} ({});",
            parts.join(", ")
        )))
    }
}

/// Generic public HTTPS reads via `httpfs` (no secret).
pub struct HttpsProvider {
    schemes: Vec<&'static str>,
    extensions: Vec<&'static str>,
}

impl Default for HttpsProvider {
    fn default() -> Self {
        Self {
            schemes: vec!["http", "https"],
            extensions: vec!["httpfs"],
        }
    }
}

impl StorageProvider for HttpsProvider {
    fn provider_id(&self) -> &str {
        "https"
    }
    fn url_schemes(&self) -> &[&str] {
        &self.schemes
    }
    fn required_extensions(&self) -> &[&str] {
        &self.extensions
    }
    fn create_secret_sql(
        &self,
        _name: &str,
        _cred: &ScopedCredential,
        _opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError> {
        Ok(None)
    }
}

/// Local files (the test/dev fake). No secret, no network — reads are confined
/// by the engine's `allowed_directories`.
pub struct LocalFileProvider;

impl StorageProvider for LocalFileProvider {
    fn provider_id(&self) -> &str {
        "local-fake"
    }
    fn url_schemes(&self) -> &[&str] {
        &["file"]
    }
    fn required_extensions(&self) -> &[&str] {
        &[]
    }
    fn create_secret_sql(
        &self,
        _name: &str,
        _cred: &ScopedCredential,
        _opts: &ProviderOptions,
    ) -> Result<Option<String>, DataSourceError> {
        Ok(None)
    }
}

/// Build a provider for a known id (extensible: add a match arm or register a
/// custom provider directly with [`ProviderRegistry::register`]).
pub fn default_provider(id: &str) -> Option<Box<dyn StorageProvider>> {
    match id {
        "s3" => Some(Box::new(S3Provider::default())),
        "az" | "azure" | "abfss" => Some(Box::new(AzureProvider::default())),
        "gcs" | "gs" => Some(Box::new(GcsProvider::default())),
        "https" | "http" => Some(Box::new(HttpsProvider::default())),
        "local-fake" | "local" => Some(Box::new(LocalFileProvider)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Registry + engine setup plan
// ---------------------------------------------------------------------------

/// A lookup of enabled storage providers by id, plus per-provider options.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Box<dyn StorageProvider>>,
    options: BTreeMap<String, ProviderOptions>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a provider.
    pub fn register(&mut self, provider: Box<dyn StorageProvider>) {
        self.providers
            .insert(provider.provider_id().to_string(), provider);
    }

    /// Set per-provider options (endpoint/region overrides).
    pub fn set_options(&mut self, provider_id: &str, opts: ProviderOptions) {
        self.options.insert(provider_id.to_string(), opts);
    }

    pub fn get(&self, id: &str) -> Option<&dyn StorageProvider> {
        self.providers.get(id).map(|b| b.as_ref())
    }

    pub fn options_for(&self, id: &str) -> ProviderOptions {
        self.options.get(id).cloned().unwrap_or_default()
    }

    /// Build the `CREATE SECRET` statement for a scoped credential, selecting
    /// the provider by `cred.provider`.
    pub fn secret_sql_for(
        &self,
        name: &str,
        cred: &ScopedCredential,
    ) -> Result<Option<String>, DataSourceError> {
        let provider = self
            .get(&cred.provider)
            .ok_or_else(|| DataSourceError::UnknownProvider(cred.provider.clone()))?;
        let opts = self.options_for(&cred.provider);
        provider.create_secret_sql(name, cred, &opts)
    }
}

/// The engine-init decision derived once from configuration. It captures the
/// secure-read boundary knobs so the untrusted query text can never widen them.
#[derive(Clone)]
pub struct StorageSetup {
    /// Master switch: allow network egress (sets `enable_external_access=true`).
    /// When `false`, the engine stays in the strict/local-scoped lockdown.
    pub enable_remote_access: bool,
    /// Extensions to pre-load (verified once at engine init).
    pub preload_extensions: Vec<String>,
    /// Local directories DuckDB may read even with external access disabled
    /// (`allowed_directories`). Empty = none (strict).
    pub allowed_local_paths: Vec<String>,
    /// Fail engine init if a pre-load extension cannot be loaded.
    pub require_extensions: bool,
    /// Encrypt DuckDB's on-disk spill (temp) files at rest (`temp_file_encryption`).
    /// Resolved at engine init: starts from config and is cleared if the running
    /// DuckDB build rejects the setting (graceful feature-detection).
    pub temp_file_encryption: bool,
    /// Enabled storage providers (for per-job secret minting).
    pub providers: Arc<ProviderRegistry>,
    /// The worker's sealing keypair, used to open **sealed** credential tokens
    /// just-in-time at engine setup (architecture §9.2/§9.3). `None` on workers
    /// that only ever receive plaintext (test/fake) credentials; sealed tokens
    /// then fail loudly with [`DataSourceError::SealingKeyUnavailable`].
    pub sealing: Option<Arc<SealingKeypair>>,
}

impl StorageSetup {
    /// The strict default: no remote access, no allowed dirs, no providers —
    /// behaves exactly like the original locked-down engine.
    pub fn strict() -> Self {
        Self {
            enable_remote_access: false,
            preload_extensions: Vec::new(),
            allowed_local_paths: Vec::new(),
            require_extensions: false,
            temp_file_encryption: true,
            providers: Arc::new(ProviderRegistry::new()),
            sealing: None,
        }
    }

    /// Attach the worker's sealing keypair so sealed credential tokens can be
    /// opened just-in-time. The same key's public half is what a requester
    /// seals credentials to (bound into the worker's attestation, §9.3).
    pub fn with_sealing(mut self, keypair: Arc<SealingKeypair>) -> Self {
        self.sealing = Some(keypair);
        self
    }

    /// Resolve a per-job credential to a **plaintext-token** [`ScopedCredential`]
    /// ready for secret minting: a sealed token is opened just-in-time with the
    /// worker's sealing key; a plaintext token passes through unchanged. The
    /// decrypted material lives only in this transient value, never at rest.
    pub fn resolve_credential(
        &self,
        cred: &ScopedCredential,
    ) -> Result<ScopedCredential, DataSourceError> {
        if !CloudCredential::is_sealed_token(&cred.token) {
            return Ok(cred.clone());
        }
        let key = self
            .sealing
            .as_ref()
            .ok_or(DataSourceError::SealingKeyUnavailable)?;
        let opened = CloudCredential::unseal(&cred.token, key)?;
        Ok(ScopedCredential {
            token: opened.to_token(),
            ..cred.clone()
        })
    }

    /// Derive the setup from a [`p2p_config::StorageConfig`]. Resolves enabled
    /// providers (+ their options), the explicit extension pre-load list, and
    /// the local allow-list — all from config, none hard-coded.
    pub fn from_config(cfg: &p2p_config::StorageConfig) -> Self {
        let mut registry = ProviderRegistry::new();
        for id in &cfg.enabled_providers {
            if let Some(p) = default_provider(id) {
                registry.register(p);
            }
        }
        // Resolve options for every enabled provider (plus any with an explicit
        // options table), layering top-level [storage] defaults (also fed by
        // `P2P_STORAGE_*` env) under the per-provider `[storage.provider_options.<id>]`
        // overrides. This is why a MinIO `s3` provider picks up endpoint /
        // url_style / use_ssl / region even with no provider_options table.
        const RESERVED: &[&str] = &["endpoint", "region", "url_style", "use_ssl"];
        let empty = BTreeMap::new();
        let ids: std::collections::BTreeSet<&String> = cfg
            .enabled_providers
            .iter()
            .chain(cfg.provider_options.keys())
            .collect();
        for id in ids {
            let kv = cfg.provider_options.get(id).unwrap_or(&empty);
            registry.set_options(
                id,
                ProviderOptions {
                    endpoint: kv.get("endpoint").cloned().or_else(|| cfg.endpoint.clone()),
                    region: kv.get("region").cloned().or_else(|| cfg.region.clone()),
                    url_style: kv
                        .get("url_style")
                        .cloned()
                        .or_else(|| cfg.url_style.clone()),
                    use_ssl: kv
                        .get("use_ssl")
                        .and_then(|v| v.trim().parse::<bool>().ok())
                        .or(cfg.use_ssl),
                    extra: kv
                        .iter()
                        .filter(|(k, _)| !RESERVED.contains(&k.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                },
            );
        }

        Self {
            enable_remote_access: cfg.enable_remote_access,
            preload_extensions: cfg.preload_extensions.clone(),
            allowed_local_paths: cfg.allowed_local_paths.clone(),
            require_extensions: cfg.require_extensions,
            temp_file_encryption: cfg.temp_file_encryption,
            providers: Arc::new(registry),
            sealing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped(provider: &str, prefix: &str, cred: &CloudCredential) -> ScopedCredential {
        ScopedCredential {
            provider: provider.to_string(),
            token: cred.to_token(),
            prefix: prefix.to_string(),
            expires_at: 0,
        }
    }

    #[test]
    fn format_extension_mapping() {
        assert!(DataFormat::Csv.required_extensions().is_empty());
        assert_eq!(DataFormat::Parquet.required_extensions(), vec!["parquet"]);
        assert_eq!(
            DataFormat::Delta.required_extensions(),
            vec!["delta", "parquet"]
        );
        assert_eq!(DataFormat::parse("DeltaLake"), DataFormat::Delta);
        assert_eq!(DataFormat::parse("orc"), DataFormat::Other("orc".into()));
    }

    #[test]
    fn s3_secret_sql_scopes_and_includes_session_token() {
        let cred = CloudCredential {
            key_id: Some("AKIA".into()),
            secret: Some("shh".into()),
            session_token: Some("sts-temp".into()),
            region: Some("eu-west-1".into()),
            ..Default::default()
        };
        let sc = scoped("s3", "my-bucket/events/2024/", &cred);
        let sql = S3Provider::default()
            .create_secret_sql("job_secret", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        assert!(sql.contains("TYPE s3"));
        assert!(sql.contains("KEY_ID 'AKIA'"));
        assert!(sql.contains("SESSION_TOKEN 'sts-temp'"));
        assert!(sql.contains("REGION 'eu-west-1'"));
        // Scope is a fully-qualified, slash-terminated URL.
        assert!(sql.contains("SCOPE 's3://my-bucket/events/2024/'"), "{sql}");
    }

    #[test]
    fn minio_s3_secret_sql_includes_endpoint_url_style_use_ssl() {
        // Per-call credential carries the S3-compatible / MinIO knobs.
        let cred = CloudCredential {
            key_id: Some("minioadmin".into()),
            secret: Some("miniosecret".into()),
            region: Some("us-east-1".into()),
            endpoint: Some("minio.local:9000".into()),
            url_style: Some("path".into()),
            use_ssl: Some(false),
            ..Default::default()
        };
        let sc = scoped("s3", "warehouse/delta/", &cred);
        let sql = S3Provider::default()
            .create_secret_sql("job_secret", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        assert!(sql.contains("TYPE s3"));
        assert!(sql.contains("ENDPOINT 'minio.local:9000'"), "{sql}");
        assert!(sql.contains("URL_STYLE 'path'"), "{sql}");
        // USE_SSL is an UNQUOTED boolean literal.
        assert!(sql.contains("USE_SSL false"), "{sql}");
        assert!(
            !sql.contains("USE_SSL 'false'"),
            "use_ssl must not be quoted: {sql}"
        );
        assert!(sql.contains("SCOPE 's3://warehouse/delta/'"), "{sql}");
    }

    #[test]
    fn provider_options_supply_minio_defaults_and_cred_overrides() {
        // Endpoint/url_style/use_ssl come from per-provider config options when
        // the per-call credential omits them; the credential wins when present.
        let opts = ProviderOptions {
            endpoint: Some("minio.local:9000".into()),
            region: Some("us-east-1".into()),
            url_style: Some("path".into()),
            use_ssl: Some(false),
            ..Default::default()
        };
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            // override use_ssl per call (TLS on for this job).
            use_ssl: Some(true),
            ..Default::default()
        };
        let sc = scoped("s3", "b/", &cred);
        let sql = S3Provider::default()
            .create_secret_sql("s", &sc, &opts)
            .unwrap()
            .unwrap();
        assert!(sql.contains("ENDPOINT 'minio.local:9000'"), "{sql}");
        assert!(sql.contains("URL_STYLE 'path'"), "{sql}");
        assert!(sql.contains("REGION 'us-east-1'"), "{sql}");
        assert!(sql.contains("USE_SSL true"), "cred overrides opts: {sql}");
    }

    #[test]
    fn extra_secret_key_is_validated_against_sql_injection() {
        use std::collections::BTreeMap;
        // A malicious `extra` key tries to break out of the option list. The key
        // is interpolated UNQUOTED, so it must be rejected (values are escaped,
        // but keys are identifiers).
        let mut extra = BTreeMap::new();
        extra.insert(
            "evil) AS x; ATTACH 'h::memory:' AS p; --".into(),
            "v".into(),
        );
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            extra,
            ..Default::default()
        };
        let sc = scoped("s3", "b/", &cred);
        let err = S3Provider::default()
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap_err();
        assert!(
            matches!(err, DataSourceError::InvalidSecretKey(_)),
            "got: {err:?}"
        );
    }

    #[test]
    fn extra_secret_key_valid_identifier_is_accepted_and_uppercased() {
        use std::collections::BTreeMap;
        let mut extra = BTreeMap::new();
        extra.insert("kms_key_id".into(), "abc".into());
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            extra,
            ..Default::default()
        };
        let sc = scoped("s3", "b/", &cred);
        let sql = S3Provider::default()
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        assert!(sql.contains("KMS_KEY_ID 'abc'"), "{sql}");
    }

    #[test]
    fn from_config_resolves_minio_options() {
        use std::collections::BTreeMap;
        let mut cfg = p2p_config::StorageConfig::default();
        cfg.enabled_providers = vec!["s3".into()];
        let mut s3 = BTreeMap::new();
        s3.insert("endpoint".into(), "minio.local:9000".into());
        s3.insert("url_style".into(), "path".into());
        s3.insert("use_ssl".into(), "false".into());
        cfg.provider_options.insert("s3".into(), s3);
        let setup = StorageSetup::from_config(&cfg);
        let opts = setup.providers.options_for("s3");
        assert_eq!(opts.endpoint.as_deref(), Some("minio.local:9000"));
        assert_eq!(opts.url_style.as_deref(), Some("path"));
        assert_eq!(opts.use_ssl, Some(false));
        // The reserved knobs are not leaked into `extra`.
        assert!(opts.extra.is_empty(), "{:?}", opts.extra);
    }

    #[test]
    fn top_level_storage_defaults_reach_s3_without_options_table() {
        // No [storage.provider_options.s3] table — top-level endpoint/url_style
        // (also fed by P2P_STORAGE_* env) must still reach the enabled provider.
        let mut cfg = p2p_config::StorageConfig::default();
        cfg.enabled_providers = vec!["s3".into()];
        cfg.endpoint = Some("minio.local:9000".into());
        cfg.url_style = Some("path".into());
        cfg.use_ssl = Some(false);
        let setup = StorageSetup::from_config(&cfg);
        let opts = setup.providers.options_for("s3");
        assert_eq!(opts.endpoint.as_deref(), Some("minio.local:9000"));
        assert_eq!(opts.url_style.as_deref(), Some("path"));
        assert_eq!(opts.use_ssl, Some(false));
    }

    #[test]
    fn sealed_credential_roundtrips_and_redacts_token() {
        use p2p_trust::SealingKeypair;
        let worker = SealingKeypair::generate();
        let cred = CloudCredential {
            key_id: Some("minioadmin".into()),
            secret: Some("super-secret-key".into()),
            endpoint: Some("minio.local:9000".into()),
            url_style: Some("path".into()),
            use_ssl: Some(false),
            ..Default::default()
        };
        let token = cred.seal_token(&worker.public_bytes());
        // The opaque token is ciphertext only — no plaintext key material.
        assert!(CloudCredential::is_sealed_token(&token));
        assert!(token.starts_with(SEALED_TOKEN_PREFIX));
        assert!(
            !token.contains("minioadmin"),
            "key id must not appear in token"
        );
        assert!(
            !token.contains("super-secret-key"),
            "secret must not appear in token"
        );
        // The worker opens it just-in-time and recovers the full credential.
        let opened = CloudCredential::unseal(&token, &worker).unwrap();
        assert_eq!(opened, cred);
    }

    #[test]
    fn sealed_token_rejected_by_plaintext_decoder() {
        use p2p_trust::SealingKeypair;
        let worker = SealingKeypair::generate();
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            ..Default::default()
        };
        let sc = ScopedCredential {
            provider: "s3".into(),
            token: cred.seal_token(&worker.public_bytes()),
            prefix: "b/".into(),
            expires_at: 0,
        };
        // from_scoped (the no-key path) must fail loudly, never mint empty creds.
        assert!(matches!(
            CloudCredential::from_scoped(&sc),
            Err(DataSourceError::SealingKeyUnavailable)
        ));
    }

    #[test]
    fn wrong_worker_key_cannot_open_sealed_credential() {
        use p2p_trust::SealingKeypair;
        let intended = SealingKeypair::generate();
        let attacker = SealingKeypair::generate();
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            ..Default::default()
        };
        let token = cred.seal_token(&intended.public_bytes());
        assert!(matches!(
            CloudCredential::unseal(&token, &attacker),
            Err(DataSourceError::SealOpenFailed)
        ));
    }

    #[test]
    fn storage_setup_unseals_credential_just_in_time() {
        use p2p_trust::SealingKeypair;
        let worker = Arc::new(SealingKeypair::generate());
        let setup = StorageSetup::strict().with_sealing(worker.clone());
        let cred = CloudCredential {
            key_id: Some("minioadmin".into()),
            secret: Some("miniosecret".into()),
            endpoint: Some("minio.local:9000".into()),
            url_style: Some("path".into()),
            use_ssl: Some(false),
            ..Default::default()
        };
        let sealed = ScopedCredential {
            provider: "s3".into(),
            token: cred.seal_token(&worker.public_bytes()),
            prefix: "bucket/delta/".into(),
            expires_at: 0,
        };
        let resolved = setup.resolve_credential(&sealed).unwrap();
        // The resolved token is now plaintext JSON carrying the same material,
        // and the prefix/provider are preserved.
        assert_eq!(resolved.provider, "s3");
        assert_eq!(resolved.prefix, "bucket/delta/");
        let opened = CloudCredential::from_scoped(&resolved).unwrap();
        assert_eq!(opened, cred);

        // A worker WITHOUT a sealing key fails closed on a sealed token.
        let no_key = StorageSetup::strict();
        assert!(matches!(
            no_key.resolve_credential(&sealed),
            Err(DataSourceError::SealingKeyUnavailable)
        ));
        // Plaintext tokens pass through resolve_credential unchanged.
        let plain = scoped("s3", "b/", &cred);
        assert_eq!(
            no_key.resolve_credential(&plain).unwrap().token,
            plain.token
        );
    }

    #[test]
    fn s3_secret_requires_key_material() {
        let sc = scoped("s3", "b/", &CloudCredential::default());
        let err = S3Provider::default()
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap_err();
        assert!(matches!(err, DataSourceError::MissingField(f, _) if f == "key_id"));
    }

    #[test]
    fn azure_uses_connection_string_and_scope() {
        let cred = CloudCredential {
            connection_string: Some("BlobEndpoint=...;SharedAccessSignature=sv=...".into()),
            ..Default::default()
        };
        let sc = scoped("az", "container/path/", &cred);
        let sql = AzureProvider::default()
            .create_secret_sql("job_secret", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        assert!(sql.contains("TYPE azure"));
        assert!(sql.contains("CONNECTION_STRING"));
        assert!(sql.contains("SCOPE 'azure://container/path/'"), "{sql}");
    }

    #[test]
    fn gcs_secret_sql() {
        let cred = CloudCredential {
            key_id: Some("GOOG1".into()),
            secret: Some("hmac".into()),
            ..Default::default()
        };
        let sc = scoped("gcs", "bucket/warehouse/", &cred);
        let sql = GcsProvider::default()
            .create_secret_sql("job_secret", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        assert!(sql.contains("TYPE gcs"));
        assert!(sql.contains("SCOPE 'gcs://bucket/warehouse/'"), "{sql}");
    }

    #[test]
    fn sql_injection_in_token_is_escaped() {
        let cred = CloudCredential {
            key_id: Some("a' ); DROP SECRET x; --".into()),
            secret: Some("s".into()),
            ..Default::default()
        };
        let sc = scoped("s3", "b/", &cred);
        let sql = S3Provider::default()
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap()
            .unwrap();
        // The embedded quote is doubled, so the statement is not broken out of.
        assert!(sql.contains("KEY_ID 'a'' ); DROP SECRET x; --'"), "{sql}");
    }

    #[test]
    fn https_and_local_need_no_secret() {
        let sc = scoped("https", "x/", &CloudCredential::default());
        assert!(HttpsProvider::default()
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap()
            .is_none());
        let sc = scoped("local-fake", "x/", &CloudCredential::default());
        assert!(LocalFileProvider
            .create_secret_sql("s", &sc, &ProviderOptions::default())
            .unwrap()
            .is_none());
    }

    #[test]
    fn rewrite_signed_urls_replaces_object_literals_only() {
        let signed = vec![
            p2p_proto::SignedInput {
                uri: "s3://b/events/a.parquet".into(),
                url: "https://signed.example/a?sig=1".into(),
            },
            p2p_proto::SignedInput {
                uri: "s3://b/events/b.parquet".into(),
                url: "https://signed.example/b?sig=2".into(),
            },
        ];
        let sql = "SELECT * FROM read_parquet('s3://b/events/a.parquet') \
                   UNION ALL SELECT * FROM read_parquet('s3://b/events/b.parquet')";
        let out = rewrite_signed_urls(sql, &signed);
        assert!(out.contains("'https://signed.example/a?sig=1'"), "{out}");
        assert!(out.contains("'https://signed.example/b?sig=2'"), "{out}");
        // The original object refs are gone — the worker reads only signed URLs.
        assert!(!out.contains("s3://b/events/a.parquet"), "{out}");
        assert!(!out.contains("s3://b/events/b.parquet"), "{out}");
    }

    #[test]
    fn rewrite_signed_urls_prefers_longer_uris_first() {
        // A URI that is a prefix of another must not shadow the longer one.
        let signed = vec![
            p2p_proto::SignedInput {
                uri: "s3://b/data".into(),
                url: "https://signed/short".into(),
            },
            p2p_proto::SignedInput {
                uri: "s3://b/data/file.parquet".into(),
                url: "https://signed/long".into(),
            },
        ];
        let out = rewrite_signed_urls("FROM 's3://b/data/file.parquet'", &signed);
        assert_eq!(out, "FROM 'https://signed/long'");
    }

    #[test]
    fn aws_uri_encode_preserves_unreserved_and_path() {
        assert_eq!(aws_uri_encode("a/b-c_d.e~f", false), "a/b-c_d.e~f");
        assert_eq!(aws_uri_encode("a/b", true), "a%2Fb");
        assert_eq!(aws_uri_encode("x y+z", false), "x%20y%2Bz");
    }

    #[test]
    fn registry_selects_provider_by_credential() {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(S3Provider::default()));
        let cred = CloudCredential {
            key_id: Some("k".into()),
            secret: Some("s".into()),
            ..Default::default()
        };
        let sc = scoped("s3", "b/p/", &cred);
        let sql = reg.secret_sql_for("job_secret", &sc).unwrap().unwrap();
        assert!(sql.contains("TYPE s3"));

        let unknown = scoped("r2", "b/", &cred);
        assert!(matches!(
            reg.secret_sql_for("job_secret", &unknown),
            Err(DataSourceError::UnknownProvider(_))
        ));
    }
}
