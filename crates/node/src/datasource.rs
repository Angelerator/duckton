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
    /// Custom endpoint (S3-compatible / MinIO / private link).
    pub endpoint: Option<String>,
    /// Any provider-specific extra `CREATE SECRET` key/values.
    pub extra: BTreeMap<String, String>,
}

impl CloudCredential {
    /// Decode the structured material from a [`ScopedCredential`]'s token.
    ///
    /// The local-fake provider issues a plain hex token (no JSON); in that case
    /// we return an empty credential, which is fine because the local-fake path
    /// does not create a cloud secret.
    pub fn from_scoped(cred: &ScopedCredential) -> Result<Self, DataSourceError> {
        let t = cred.token.trim();
        if t.is_empty() || !t.starts_with('{') {
            return Ok(CloudCredential::default());
        }
        serde_json::from_str(t).map_err(|e| DataSourceError::MalformedToken(e.to_string()))
    }

    /// Encode to the opaque token string (used by fake providers / requesters).
    pub fn to_token(&self) -> String {
        serde_json::to_string(self).expect("CloudCredential serializes")
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
        let region = c.region.or_else(|| opts.region.clone());
        let endpoint = c.endpoint.or_else(|| opts.endpoint.clone());

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
        for (k, v) in &c.extra {
            parts.push(format!("{} {}", k.to_uppercase(), sql_lit(v)));
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
            parts.push(format!("{} {}", k.to_uppercase(), sql_lit(v)));
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
            parts.push(format!("{} {}", k.to_uppercase(), sql_lit(v)));
        }
        parts.push(format!("SCOPE {}", sql_lit(&scope_url("gcs", &cred.prefix))));
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
    /// Enabled storage providers (for per-job secret minting).
    pub providers: Arc<ProviderRegistry>,
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
            providers: Arc::new(ProviderRegistry::new()),
        }
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
        for (id, kv) in &cfg.provider_options {
            registry.set_options(
                id,
                ProviderOptions {
                    endpoint: kv.get("endpoint").cloned().or_else(|| cfg.endpoint.clone()),
                    region: kv.get("region").cloned().or_else(|| cfg.region.clone()),
                    extra: kv
                        .iter()
                        .filter(|(k, _)| k.as_str() != "endpoint" && k.as_str() != "region")
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
            providers: Arc::new(registry),
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
        assert_eq!(DataFormat::Delta.required_extensions(), vec!["delta", "parquet"]);
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
