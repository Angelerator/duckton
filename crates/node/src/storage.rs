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
}
