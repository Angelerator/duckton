//! Object-storage credential provider + encrypted-at-rest plumbing + the
//! attestation-gated key-release flow (architecture §9.2, §9.3).
//!
//! ## What is real vs. mocked
//! * **Scoped credentials** ([`StorageCredentialProvider`]): the trait shape
//!   matches STS/SAS/downscoped-token issuance; [`LocalFakeStorage`] issues
//!   short-lived tokens scoped to a local path prefix (no cloud creds in tests).
//! * **Encrypted-at-rest** ([`EncryptedObjectStore`]): real ChaCha20-Poly1305
//!   encryption of object bytes on local files, standing in for Parquet Modular
//!   Encryption (which is engine-side and needs DuckDB/Parquet).
//! * **Attestation-gated key release** ([`KeyRelease`]): real X25519 sealing; the
//!   *attestation* is via the mock attestor (see `p2p_trust::attestation`). Real
//!   TEE hardware attestation plugs in behind the same `Attestor`/`Verifier`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use p2p_proto::{Attestation, ScopedCredential};
use p2p_trust::attestation::{AttestError, AttestationVerifier, Attestor};
use p2p_trust::{decrypt_at_rest, encrypt_at_rest, seal_to, SealedBlob, SealingKeypair};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Issues scoped, short-lived storage credentials (architecture §9.2). Pluggable
/// so S3/Azure/GCS providers slot in behind the same trait.
pub trait StorageCredentialProvider: Send + Sync {
    fn issue(&self, prefix: &str, ttl_secs: u64) -> ScopedCredential;
    fn provider_id(&self) -> &str;
}

/// A local fake that issues path-scoped tokens (no real cloud).
pub struct LocalFakeStorage {
    root: PathBuf,
}

impl LocalFakeStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl StorageCredentialProvider for LocalFakeStorage {
    fn issue(&self, prefix: &str, ttl_secs: u64) -> ScopedCredential {
        let mut token = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut token);
        ScopedCredential {
            provider: "local-fake".into(),
            token: hex::encode(token),
            prefix: prefix.to_string(),
            expires_at: now_secs() + ttl_secs,
        }
    }
    fn provider_id(&self) -> &str {
        "local-fake"
    }
}

/// Fake cloud credential providers (requester side). These mimic the real
/// short-lived, scoped issuance flows (AWS STS `AssumeRole` session tokens,
/// Azure user-delegation SAS, GCS downscoped/HMAC tokens) by emitting a
/// [`ScopedCredential`] whose opaque `token` carries a JSON
/// [`CloudCredential`](crate::datasource::CloudCredential). They issue *fake*
/// material so the per-job secret wiring can be exercised without real cloud
/// accounts; swap in a real STS/SAS client behind the same trait for production.
pub struct FakeStsS3Provider {
    region: String,
}

impl FakeStsS3Provider {
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
        }
    }
}

impl StorageCredentialProvider for FakeStsS3Provider {
    fn issue(&self, prefix: &str, ttl_secs: u64) -> ScopedCredential {
        let cred = crate::datasource::CloudCredential {
            key_id: Some(format!("ASIA{}", rand_hex(8))),
            secret: Some(rand_hex(20)),
            // The short-lived STS session token is the crux of "scoped & dies
            // in minutes" — a stolen credential is useless after expiry.
            session_token: Some(rand_hex(32)),
            region: Some(self.region.clone()),
            ..Default::default()
        };
        ScopedCredential {
            provider: "s3".into(),
            token: cred.to_token(),
            prefix: prefix.to_string(),
            expires_at: now_secs() + ttl_secs,
        }
    }
    fn provider_id(&self) -> &str {
        "s3"
    }
}

/// Fake Azure user-delegation SAS provider.
pub struct FakeAzureSasProvider;

impl StorageCredentialProvider for FakeAzureSasProvider {
    fn issue(&self, prefix: &str, ttl_secs: u64) -> ScopedCredential {
        let expiry = now_secs() + ttl_secs;
        // A path-scoped, read-only ("sp=r"), short-expiry SAS string.
        let sas = format!("sv=2024-11-04&sp=r&sr=c&se={expiry}&sig={}", rand_hex(16));
        let cred = crate::datasource::CloudCredential {
            connection_string: Some(format!(
                "BlobEndpoint=https://acct.blob.core.windows.net;SharedAccessSignature={sas}"
            )),
            ..Default::default()
        };
        ScopedCredential {
            provider: "az".into(),
            token: cred.to_token(),
            prefix: prefix.to_string(),
            expires_at: expiry,
        }
    }
    fn provider_id(&self) -> &str {
        "az"
    }
}

/// Fake GCS downscoped/HMAC provider (S3-compatible interop).
pub struct FakeGcsProvider;

impl StorageCredentialProvider for FakeGcsProvider {
    fn issue(&self, prefix: &str, ttl_secs: u64) -> ScopedCredential {
        let cred = crate::datasource::CloudCredential {
            key_id: Some(format!("GOOG1{}", rand_hex(8))),
            secret: Some(rand_hex(20)),
            session_token: Some(rand_hex(24)),
            ..Default::default()
        };
        ScopedCredential {
            provider: "gcs".into(),
            token: cred.to_token(),
            prefix: prefix.to_string(),
            expires_at: now_secs() + ttl_secs,
        }
    }
    fn provider_id(&self) -> &str {
        "gcs"
    }
}

/// Build the requester-side per-job credential issuer matching the configured
/// storage provider (architecture §9.2). Returns `None` for `local-fake` /
/// unknown ids (the local path needs no issued credential). The cloud arms
/// return the FAKE issuers shipped in this crate — they mint correctly-shaped,
/// short-lived, scoped tokens so the end-to-end wiring runs, but a REAL
/// deployment must swap in a live STS / SAS / downscoped-token client behind the
/// same [`StorageCredentialProvider`] trait.
pub fn default_credential_provider(
    cfg: &p2p_config::StorageConfig,
) -> Option<Arc<dyn StorageCredentialProvider>> {
    match cfg.provider.trim().to_ascii_lowercase().as_str() {
        "s3" => Some(Arc::new(FakeStsS3Provider::new(
            cfg.region.clone().unwrap_or_else(|| "us-east-1".into()),
        ))),
        "az" | "azure" | "abfss" => Some(Arc::new(FakeAzureSasProvider)),
        "gcs" | "gs" => Some(Arc::new(FakeGcsProvider)),
        _ => None,
    }
}

/// **Requester side.** Build a per-job [`ScopedCredential`] whose opaque token
/// carries `cred` **sealed** (X25519 + ChaCha20-Poly1305) to a worker's
/// `worker_sealing_pub` key, scoped to `prefix` for `ttl_secs` (architecture
/// §9.2/§9.3).
///
/// This is the encrypted-credentials path for self-hosted / MinIO S3-compatible
/// stores: the requester puts the MinIO access key + secret in a
/// [`CloudCredential`] (with `endpoint`/`url_style`/`use_ssl`), seals it to the
/// **selected** worker's sealing public key — learned from that worker's
/// attestation quote (`Enclave::attest` binds the key) or its signed capability
/// record — and dispatches it. The plaintext never travels or persists; the
/// worker decrypts it just-in-time at engine setup via
/// [`StorageSetup::resolve_credential`](crate::datasource::StorageSetup::resolve_credential).
///
/// `provider` is the storage provider id the worker will mint a secret for
/// (e.g. `"s3"` for MinIO / S3-compatible, `"gcs"`).
pub fn sealed_credential(
    provider: &str,
    worker_sealing_pub: &[u8; 32],
    cred: &crate::datasource::CloudCredential,
    prefix: &str,
    ttl_secs: u64,
) -> ScopedCredential {
    ScopedCredential {
        provider: provider.to_string(),
        token: cred.seal_token(worker_sealing_pub),
        prefix: prefix.to_string(),
        expires_at: now_secs() + ttl_secs,
    }
}

fn rand_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    hex::encode(buf)
}

// ---------------------------------------------------------------------------
// Presigned-URL credential mode (requester side, architecture §9.2)
// ---------------------------------------------------------------------------

/// Produces **presigned, time-limited read URLs** for a job's pinned input
/// objects (presigned credential mode). The REQUESTER holds the cloud
/// credentials and signs a narrow, per-object HTTPS URL; the commodity WORKER
/// reads it with plain HTTPS and **no secret on the host** (DuckDB
/// `read_parquet('<signed https url>')`, no `CREATE SECRET`).
///
/// Pluggable so S3 SigV4 / Azure user-delegation SAS / GCS signed-URL signers
/// slot in behind one trait, exactly like [`StorageCredentialProvider`].
///
/// ## Honest scope
/// A presigned URL is a **bearer** artifact: anyone holding it can re-read that
/// one object until it expires. That is expected and is the deliberate trade —
/// the win is that **no reusable credential** (access key / session token) ever
/// reaches the worker, only an expiring read capability scoped to one object.
pub trait PresignProvider: Send + Sync {
    /// Provider id (matches the storage `ProviderRegistry` ids: `s3` / `az` /
    /// `gcs` / …, or `fake-presign` for the no-cloud deterministic signer).
    fn provider_id(&self) -> &str;

    /// Sign `uri` (e.g. `s3://bucket/key.parquet`) for `ttl_secs`, returning the
    /// presigned HTTPS URL a worker can read with no credential.
    fn presign(
        &self,
        uri: &str,
        ttl_secs: u64,
    ) -> Result<String, crate::datasource::DataSourceError>;
}

/// A no-cloud, deterministic presigner used for the open-grid default and tests.
///
/// It emits a correctly-SHAPED, per-object, expiring signed URL form (host +
/// object path + `X-Amz-Expires` + a deterministic `X-Amz-Signature`) WITHOUT
/// real cloud crypto, so the end-to-end presigned wiring (coordinator → dispatch
/// → worker rewrite → no-secret HTTPS read) can be exercised with no cloud
/// account. Swap in [`S3PresignProvider`] (or an Azure/GCS signer) for a real
/// deployment. The signature is a deterministic function of the URI ONLY (not the
/// wall clock), so two requesters signing the same object agree — handy for
/// tests — while the embedded `X-Amz-Expires`/`X-Amz-Date` still carry the TTL.
pub struct FakePresignProvider {
    host: String,
}

impl Default for FakePresignProvider {
    fn default() -> Self {
        Self {
            host: "presigned.local".to_string(),
        }
    }
}

impl FakePresignProvider {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PresignProvider for FakePresignProvider {
    fn provider_id(&self) -> &str {
        "fake-presign"
    }

    fn presign(
        &self,
        uri: &str,
        ttl_secs: u64,
    ) -> Result<String, crate::datasource::DataSourceError> {
        // Drop the scheme, keep `host_or_bucket/key…` as the object path so the
        // signed URL stays per-object and recognizable.
        let path = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
        let path = path.trim_start_matches('/');
        let expires_at = now_secs() + ttl_secs;
        // Deterministic per-object signature (URI-only) — no real crypto.
        let mut h = blake3::Hasher::new();
        h.update(b"duckdb-p2p-presign-v1");
        h.update(uri.as_bytes());
        let sig = hex::encode(&h.finalize().as_bytes()[..16]);
        Ok(format!(
            "https://{}/{}?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Expires={}&X-Amz-Date={}&X-Amz-SignedHeaders=host&X-Amz-Signature={}",
            self.host,
            crate::datasource::aws_uri_encode(path, false),
            ttl_secs,
            expires_at,
            sig
        ))
    }
}

/// Real AWS S3 (and S3-compatible / MinIO) **SigV4 query-string presigner**.
///
/// Given the requester's S3 credentials it signs a per-object, time-limited GET
/// URL (`X-Amz-Algorithm`/`X-Amz-Credential`/`X-Amz-Date`/`X-Amz-Expires`/
/// `X-Amz-SignedHeaders`/`X-Amz-Signature`, `UNSIGNED-PAYLOAD`). The worker reads
/// it with plain HTTPS and no secret. Virtual-hosted URLs are produced for AWS;
/// when an `endpoint` is set (MinIO / S3-compatible) path-style URLs against that
/// host are produced instead.
///
/// The credentials live ONLY on the requester (never shipped to the worker), so
/// this is constructed by the operator and injected — `default_presign_provider`
/// returns the no-cloud [`FakePresignProvider`] because secrets are deliberately
/// never read from config.
pub struct S3PresignProvider {
    access_key: String,
    secret_key: String,
    region: String,
    /// Optional S3-compatible endpoint host (e.g. `minio.local:9000`); when set,
    /// path-style URLs are produced. `None` ⇒ virtual-hosted AWS URLs.
    endpoint: Option<String>,
    /// HTTPS (true, default) vs plain HTTP (MinIO dev).
    use_ssl: bool,
}

impl S3PresignProvider {
    pub fn new(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            region: region.into(),
            endpoint: None,
            use_ssl: true,
        }
    }

    /// Set an S3-compatible endpoint (MinIO) and TLS flag (path-style URLs).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>, use_ssl: bool) -> Self {
        self.endpoint = Some(endpoint.into());
        self.use_ssl = use_ssl;
        self
    }

    /// Sign at an explicit `now` (Unix seconds) — the testable core of
    /// [`PresignProvider::presign`].
    pub fn presign_at(
        &self,
        uri: &str,
        ttl_secs: u64,
        now: u64,
    ) -> Result<String, crate::datasource::DataSourceError> {
        use crate::datasource::{aws_uri_encode, DataSourceError};

        let rest = uri.split_once("://").map(|(_, r)| r).unwrap_or(uri);
        let (bucket, key) = rest.split_once('/').ok_or_else(|| {
            DataSourceError::MalformedToken(format!("s3 uri missing bucket/key: {uri}"))
        })?;
        if bucket.is_empty() || key.is_empty() {
            return Err(DataSourceError::MalformedToken(format!(
                "s3 uri missing bucket/key: {uri}"
            )));
        }

        let scheme = if self.use_ssl { "https" } else { "http" };
        let (host, canonical_uri) = match &self.endpoint {
            // Path-style for an S3-compatible endpoint (MinIO).
            Some(ep) => (
                ep.clone(),
                format!("/{}/{}", bucket, aws_uri_encode(key, false)),
            ),
            // Virtual-hosted AWS URL.
            None => (
                format!("{bucket}.s3.{}.amazonaws.com", self.region),
                format!("/{}", aws_uri_encode(key, false)),
            ),
        };

        let (amz_date, datestamp) = format_amz_date(now);
        let credential_scope = format!("{datestamp}/{}/s3/aws4_request", self.region);
        let credential = format!("{}/{}", self.access_key, credential_scope);

        // Canonical query string: params sorted by key, each URL-encoded.
        let mut params: Vec<(String, String)> = vec![
            ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
            ("X-Amz-Credential".into(), credential),
            ("X-Amz-Date".into(), amz_date.clone()),
            ("X-Amz-Expires".into(), ttl_secs.to_string()),
            ("X-Amz-SignedHeaders".into(), "host".into()),
        ];
        params.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_query = params
            .iter()
            .map(|(k, v)| format!("{}={}", aws_uri_encode(k, true), aws_uri_encode(v, true)))
            .collect::<Vec<_>>()
            .join("&");

        let canonical_headers = format!("host:{host}\n");
        let signed_headers = "host";
        let payload_hash = "UNSIGNED-PAYLOAD";
        let canonical_request = format!(
            "GET\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );

        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );

        let signing_key = sigv4_signing_key(&self.secret_key, &datestamp, &self.region, "s3");
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        Ok(format!(
            "{scheme}://{host}{canonical_uri}?{canonical_query}&X-Amz-Signature={signature}"
        ))
    }
}

impl PresignProvider for S3PresignProvider {
    fn provider_id(&self) -> &str {
        "s3"
    }

    fn presign(
        &self,
        uri: &str,
        ttl_secs: u64,
    ) -> Result<String, crate::datasource::DataSourceError> {
        self.presign_at(uri, ttl_secs, now_secs())
    }
}

/// Build the requester-side presigner for the presigned credential mode. Returns
/// the no-cloud, deterministic [`FakePresignProvider`] — real S3/Azure/GCS
/// signers need the requester's cloud credentials, which (by design) are NEVER
/// read from config; an operator constructs [`S3PresignProvider`] (etc.) and
/// injects it with `Coordinator::with_presign_provider`.
pub fn default_presign_provider(
    _cfg: &p2p_config::StorageConfig,
) -> Option<Arc<dyn PresignProvider>> {
    Some(Arc::new(FakePresignProvider::new()))
}

/// HMAC-SHA256(key, msg).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// Lowercase hex SHA-256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Derive the SigV4 signing key: HMAC chain over date → region → service →
/// `aws4_request`, seeded with `"AWS4" + secret`.
fn sigv4_signing_key(secret: &str, datestamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Format a Unix timestamp as the SigV4 `(amz_date, datestamp)` pair in UTC:
/// `("YYYYMMDDTHHMMSSZ", "YYYYMMDD")`. Uses Hinnant's civil-from-days algorithm
/// so no date/time crate dependency is needed.
fn format_amz_date(epoch_secs: u64) -> (String, String) {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    (
        format!("{y:04}{mo:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{y:04}{mo:02}{d:02}"),
    )
}

/// Convert days since the Unix epoch (1970-01-01) to a `(year, month, day)`
/// civil date (Howard Hinnant's public-domain algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// Encrypted-at-rest object store over local files (Parquet-Modular-Encryption
/// stand-in). Each object is encrypted with a per-object 32-byte data key.
pub struct EncryptedObjectStore {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decryption failed (wrong key or tampered ciphertext)")]
    Decrypt,
}

impl EncryptedObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Write `plaintext` encrypted at rest under `object_key` with `data_key`.
    pub fn put(
        &self,
        object_key: &str,
        data_key: &[u8; 32],
        plaintext: &[u8],
    ) -> Result<(), StorageError> {
        std::fs::create_dir_all(&self.root)?;
        let blob = encrypt_at_rest(data_key, plaintext);
        std::fs::write(self.path(object_key), blob)?;
        Ok(())
    }

    /// Read + decrypt an object. Fails if the data key is wrong.
    pub fn get(&self, object_key: &str, data_key: &[u8; 32]) -> Result<Vec<u8>, StorageError> {
        let blob = std::fs::read(self.path(object_key))?;
        decrypt_at_rest(data_key, &blob).ok_or(StorageError::Decrypt)
    }
}

/// An "enclave": holds a sealing keypair and an attestor that binds the sealing
/// public key into attestation evidence (architecture §9.3).
pub struct Enclave {
    sealing: SealingKeypair,
    attestor: Arc<dyn Attestor>,
}

impl Enclave {
    pub fn new(attestor: Arc<dyn Attestor>) -> Self {
        Self {
            sealing: SealingKeypair::generate(),
            attestor,
        }
    }

    /// The enclave's sealing public key (data keys are sealed to this).
    pub fn sealing_pubkey(&self) -> [u8; 32] {
        self.sealing.public_bytes()
    }

    /// Produce an attestation binding the sealing key, freshened by `nonce`.
    pub fn attest(&self, nonce: &[u8]) -> Attestation {
        self.attestor.produce(nonce, &self.sealing.public_bytes())
    }

    /// Open a sealed data key inside the enclave.
    pub fn open_key(&self, blob: &SealedBlob) -> Option<[u8; 32]> {
        let bytes = self.sealing.open(blob)?;
        bytes.try_into().ok()
    }
}

/// Requester-side attestation-gated key release.
pub struct KeyRelease<V: AttestationVerifier> {
    verifier: V,
}

impl<V: AttestationVerifier> KeyRelease<V> {
    pub fn new(verifier: V) -> Self {
        Self { verifier }
    }

    /// Release (seal) `data_key` to the enclave's sealing key **only if** the
    /// attestation verifies (correct measurement, level, nonce, bound key).
    pub fn release(
        &self,
        attestation: &Attestation,
        nonce: &[u8],
        enclave_sealing_pub: &[u8; 32],
        data_key: &[u8; 32],
    ) -> Result<SealedBlob, AttestError> {
        self.verifier
            .verify(attestation, nonce, enclave_sealing_pub)?;
        Ok(seal_to(enclave_sealing_pub, data_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use p2p_proto::AttestationLevel;
    use p2p_trust::{AllowlistVerifier, MockAttestor};
    use rand::rngs::OsRng;
    use tempfile::tempdir;

    #[test]
    fn scoped_credential_is_short_lived_and_prefixed() {
        let s = LocalFakeStorage::new("/tmp/fake");
        let cred = s.issue("events/2024/", 900);
        assert_eq!(cred.provider, "local-fake");
        assert_eq!(cred.prefix, "events/2024/");
        assert!(cred.expires_at > now_secs());
        assert!(cred.expires_at <= now_secs() + 900);
    }

    #[test]
    fn default_credential_provider_maps_cloud_ids_only() {
        let mut cfg = p2p_config::StorageConfig::default();
        // local-fake (default) ⇒ no issued credential (local path needs none).
        assert!(default_credential_provider(&cfg).is_none());
        cfg.provider = "s3".into();
        assert_eq!(
            default_credential_provider(&cfg).unwrap().provider_id(),
            "s3"
        );
        cfg.provider = "gcs".into();
        assert_eq!(
            default_credential_provider(&cfg).unwrap().provider_id(),
            "gcs"
        );
        cfg.provider = "az".into();
        assert_eq!(
            default_credential_provider(&cfg).unwrap().provider_id(),
            "az"
        );
    }

    #[test]
    fn encrypted_object_store_roundtrip() {
        let dir = tempdir().unwrap();
        let store = EncryptedObjectStore::new(dir.path());
        let key = [7u8; 32];
        store
            .put("part-0.parquet", &key, b"hello columnar world")
            .unwrap();
        let got = store.get("part-0.parquet", &key).unwrap();
        assert_eq!(got, b"hello columnar world");
        // wrong key cannot read
        assert!(matches!(
            store.get("part-0.parquet", &[0u8; 32]),
            Err(StorageError::Decrypt)
        ));
    }

    fn enclave_and_verifier(
        measurement: &str,
        allowed: &str,
        level: AttestationLevel,
        required: AttestationLevel,
    ) -> (Enclave, KeyRelease<AllowlistVerifier>) {
        let authority = SigningKey::generate(&mut OsRng);
        let authority_pub = authority.verifying_key().to_bytes();
        let attestor = Arc::new(MockAttestor::new(authority, measurement.to_string(), level));
        let enclave = Enclave::new(attestor);
        let verifier = AllowlistVerifier::new(authority_pub, [allowed.to_string()], required);
        (enclave, KeyRelease::new(verifier))
    }

    #[test]
    fn key_released_to_attested_enclave_and_opened() {
        let (enclave, release) = enclave_and_verifier(
            "duckdb-enclave-v1",
            "duckdb-enclave-v1",
            AttestationLevel::L2,
            AttestationLevel::L2,
        );
        let nonce = [1u8; 16];
        let att = enclave.attest(&nonce);
        let data_key = [42u8; 32];

        let sealed = release
            .release(&att, &nonce, &enclave.sealing_pubkey(), &data_key)
            .expect("attested release succeeds");
        assert_eq!(enclave.open_key(&sealed), Some(data_key));
    }

    #[test]
    fn key_release_denied_for_unlisted_measurement() {
        let (enclave, release) = enclave_and_verifier(
            "rogue-image",
            "duckdb-enclave-v1",
            AttestationLevel::L2,
            AttestationLevel::L2,
        );
        let nonce = [1u8; 16];
        let att = enclave.attest(&nonce);
        let r = release.release(&att, &nonce, &enclave.sealing_pubkey(), &[42u8; 32]);
        assert!(matches!(r, Err(AttestError::MeasurementNotAllowed)));
    }

    #[test]
    fn key_release_denied_for_insufficient_level() {
        let (enclave, release) = enclave_and_verifier(
            "duckdb-enclave-v1",
            "duckdb-enclave-v1",
            AttestationLevel::L1, // enclave only L1
            AttestationLevel::L2, // policy needs L2
        );
        let nonce = [1u8; 16];
        let att = enclave.attest(&nonce);
        let r = release.release(&att, &nonce, &enclave.sealing_pubkey(), &[42u8; 32]);
        assert!(matches!(r, Err(AttestError::LevelTooLow { .. })));
    }

    #[test]
    fn end_to_end_sealed_key_decrypts_encrypted_object() {
        // Encrypt a local "parquet" object at rest, then release the key to an
        // attested enclave which decrypts it.
        let dir = tempdir().unwrap();
        let store = EncryptedObjectStore::new(dir.path());
        let data_key = [99u8; 32];
        store
            .put("secret.parquet", &data_key, b"sensitive rows")
            .unwrap();

        let (enclave, release) = enclave_and_verifier(
            "duckdb-enclave-v1",
            "duckdb-enclave-v1",
            AttestationLevel::L2,
            AttestationLevel::L2,
        );
        let nonce = [5u8; 16];
        let att = enclave.attest(&nonce);
        let sealed = release
            .release(&att, &nonce, &enclave.sealing_pubkey(), &data_key)
            .unwrap();

        // Inside the enclave: open the key, then decrypt the object.
        let opened = enclave.open_key(&sealed).unwrap();
        let plaintext = store.get("secret.parquet", &opened).unwrap();
        assert_eq!(plaintext, b"sensitive rows");
    }

    // ---- Presigned credential mode ----

    #[test]
    fn fake_presign_yields_per_object_scoped_urls() {
        let p = FakePresignProvider::new();
        let a = p.presign("s3://bucket/events/2024/a.parquet", 900).unwrap();
        let b = p.presign("s3://bucket/events/2024/b.parquet", 900).unwrap();
        // Each URL is HTTPS, scoped to its own object path, and distinct.
        assert!(a.starts_with("https://"));
        assert!(a.contains("bucket/events/2024/a.parquet"), "{a}");
        assert!(b.contains("bucket/events/2024/b.parquet"), "{b}");
        assert_ne!(a, b, "each object gets a distinct signed URL");
        // Time-limited bearer artifact: carries an expiry + a signature, no creds.
        assert!(a.contains("X-Amz-Expires=900"), "{a}");
        assert!(a.contains("X-Amz-Signature="), "{a}");
        assert!(
            !a.contains("secret"),
            "no credential material in the URL: {a}"
        );
        // Deterministic per-object signature (URI-only) ⇒ two signs agree.
        assert_eq!(
            p.presign("s3://bucket/events/2024/a.parquet", 900).unwrap(),
            a
        );
    }

    #[test]
    fn s3_presign_sigv4_is_well_formed_and_scoped() {
        // Virtual-hosted AWS URL, fixed clock for determinism.
        let p = S3PresignProvider::new("AKIDEXAMPLE", "secretkey", "us-east-1");
        let url = p
            .presign_at("s3://my-bucket/data/part-0.parquet", 600, 1_700_000_000)
            .unwrap();
        assert!(
            url.starts_with("https://my-bucket.s3.us-east-1.amazonaws.com/data/part-0.parquet?")
        );
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(url.contains("X-Amz-Expires=600"));
        assert!(url.contains("X-Amz-Credential=AKIDEXAMPLE%2F"));
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        assert!(url.contains("X-Amz-Signature="));
        // Deterministic at a fixed clock (the whole point of `presign_at`).
        assert_eq!(
            p.presign_at("s3://my-bucket/data/part-0.parquet", 600, 1_700_000_000)
                .unwrap(),
            url
        );
        // A different object signs differently (per-object scope).
        let other = p
            .presign_at("s3://my-bucket/data/part-1.parquet", 600, 1_700_000_000)
            .unwrap();
        assert_ne!(other, url);
    }

    #[test]
    fn s3_presign_path_style_for_minio_endpoint() {
        let p = S3PresignProvider::new("minioadmin", "miniosecret", "us-east-1")
            .with_endpoint("minio.local:9000", false);
        let url = p
            .presign_at("s3://warehouse/delta/part.parquet", 300, 1_700_000_000)
            .unwrap();
        // Path-style against the endpoint host, plain HTTP (use_ssl=false).
        assert!(
            url.starts_with("http://minio.local:9000/warehouse/delta/part.parquet?"),
            "{url}"
        );
        assert!(url.contains("X-Amz-Signature="));
    }

    #[test]
    fn default_presign_provider_is_the_fake_no_cloud_signer() {
        let cfg = p2p_config::StorageConfig::default();
        let p = default_presign_provider(&cfg).expect("a presigner is always available");
        assert_eq!(p.provider_id(), "fake-presign");
    }

    #[test]
    fn amz_date_formats_known_instant() {
        // 2023-11-14T22:13:20Z is Unix 1_700_000_000.
        let (amz, date) = format_amz_date(1_700_000_000);
        assert_eq!(amz, "20231114T221320Z");
        assert_eq!(date, "20231114");
    }
}
