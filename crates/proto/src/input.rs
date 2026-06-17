//! Deterministic-input pinning (source-data-drift verification).
//!
//! Quorum verification compares result hashes across redundant replicas. If the
//! external source data (S3 / ADLS / GCS objects, Delta / Iceberg snapshots)
//! changes *between* concurrent replica executions, an honest minority that read
//! the newer bytes produces a different result hash and would otherwise be
//! mis-flagged as wrong. To make redundant execution comparable, the coordinator
//! pins a concrete, version-identified manifest of the inputs at dispatch time
//! and ships it inside the [`crate::Dispatch`]. Each worker reports back (in its
//! [`crate::ResultCommit`]) the fingerprint of the snapshot it actually read.
//!
//! This module owns the wire types and the **deterministic fingerprint**: two
//! coordinators given the same pinned object set always derive the same
//! fingerprint, and any change to a pinned object's version flips it.

use serde::{Deserialize, Serialize};

/// Domain-separation prefix so an input fingerprint can never collide with the
/// result hash, query hash, or node id (all distinct BLAKE3 uses in the system).
const FINGERPRINT_DOMAIN: &[u8] = b"duckdb-p2p-input-fingerprint-v1";

/// The version identity of one pinned external object, by provider. The exact
/// fields mirror what each provider exposes for immutability:
///  * **S3**: `versionId` (object versioning), `ETag`, and `size` — on an
///    unversioned bucket the `version_id` is `None` and the pin is a best-effort
///    `ETag` + `size` pin.
///  * **Azure**: blob `ETag` + optional `versionId`.
///  * **GCS**: object `generation` number (monotonic, immutable per write).
///  * **ContentAddressed**: a direct content `sha256` (local files / immutable
///    object stores).
///  * **Lakehouse**: a Delta/Iceberg table snapshot (`format` + `snapshot_id`),
///    which pins the whole table version without enumerating every data file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectVersion {
    S3 {
        version_id: Option<String>,
        etag: Option<String>,
        size: u64,
    },
    Azure {
        etag: Option<String>,
        version_id: Option<String>,
    },
    Gcs {
        generation: Option<i64>,
    },
    ContentAddressed {
        sha256: String,
    },
    Lakehouse {
        format: String,
        snapshot_id: String,
    },
}

impl ObjectVersion {
    /// Whether this pin is a **concrete** immutable identifier (a version id,
    /// generation, content hash, or table snapshot) versus a best-effort pin
    /// (e.g. only an `ETag`/`size` on an unversioned bucket, or no identifier at
    /// all). Best-effort pins still detect most drift but cannot guarantee it.
    pub fn is_concrete(&self) -> bool {
        match self {
            ObjectVersion::S3 { version_id, .. } => version_id.is_some(),
            ObjectVersion::Azure { version_id, .. } => version_id.is_some(),
            ObjectVersion::Gcs { generation } => generation.is_some(),
            ObjectVersion::ContentAddressed { sha256 } => !sha256.is_empty(),
            ObjectVersion::Lakehouse { snapshot_id, .. } => !snapshot_id.is_empty(),
        }
    }

    /// Append the canonical, tagged + length-prefixed bytes of this version to
    /// `buf` (used by [`compute_fingerprint`]). A stable tag per variant plus
    /// length-prefixed fields means two distinct versions can never collide.
    fn fingerprint_into(&self, buf: &mut Vec<u8>) {
        let opt = |buf: &mut Vec<u8>, s: &Option<String>| match s {
            Some(v) => {
                buf.push(1);
                push_bytes(buf, v.as_bytes());
            }
            None => buf.push(0),
        };
        match self {
            ObjectVersion::S3 {
                version_id,
                etag,
                size,
            } => {
                buf.push(1);
                opt(buf, version_id);
                opt(buf, etag);
                buf.extend_from_slice(&size.to_le_bytes());
            }
            ObjectVersion::Azure { etag, version_id } => {
                buf.push(2);
                opt(buf, etag);
                opt(buf, version_id);
            }
            ObjectVersion::Gcs { generation } => {
                buf.push(3);
                match generation {
                    Some(g) => {
                        buf.push(1);
                        buf.extend_from_slice(&g.to_le_bytes());
                    }
                    None => buf.push(0),
                }
            }
            ObjectVersion::ContentAddressed { sha256 } => {
                buf.push(4);
                push_bytes(buf, sha256.as_bytes());
            }
            ObjectVersion::Lakehouse {
                format,
                snapshot_id,
            } => {
                buf.push(5);
                push_bytes(buf, format.as_bytes());
                push_bytes(buf, snapshot_id.as_bytes());
            }
        }
    }
}

/// One concrete, version-pinned object in an [`InputSnapshot`] manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedObject {
    /// The fully-qualified object URI (e.g. `s3://bucket/key.parquet`).
    pub uri: String,
    /// The storage provider id that resolved it (`s3` / `az` / `gcs` /
    /// `local-fake` / …).
    pub provider: String,
    /// The provider-specific immutable version identity.
    pub version: ObjectVersion,
}

/// A pinned manifest of the external inputs a job reads, plus a deterministic
/// fingerprint over the (sorted) manifest. Attached to a [`crate::Dispatch`] and
/// echoed back (by fingerprint) in each [`crate::ResultCommit`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputSnapshot {
    /// `BLAKE3(domain ‖ canonical sorted manifest)` as lowercase hex.
    pub fingerprint: String,
    /// The pinned objects, sorted by `(uri, provider)` for determinism.
    pub objects: Vec<PinnedObject>,
}

impl InputSnapshot {
    /// Build a snapshot from a set of pinned objects, sorting the manifest and
    /// computing the deterministic [`fingerprint`](Self::fingerprint).
    pub fn from_objects(mut objects: Vec<PinnedObject>) -> Self {
        objects.sort_by(|a, b| a.uri.cmp(&b.uri).then_with(|| a.provider.cmp(&b.provider)));
        let fingerprint = compute_fingerprint(&objects);
        Self {
            fingerprint,
            objects,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Whether every pinned object carries a concrete (not best-effort) version.
    pub fn fully_concrete(&self) -> bool {
        self.objects.iter().all(|o| o.version.is_concrete())
    }
}

/// Compute the deterministic input fingerprint over an object manifest. The
/// manifest is sorted internally, so the result is independent of the order the
/// objects were enumerated in (glob/list order is not stable across providers).
pub fn compute_fingerprint(objects: &[PinnedObject]) -> String {
    let mut sorted: Vec<&PinnedObject> = objects.iter().collect();
    sorted.sort_by(|a, b| a.uri.cmp(&b.uri).then_with(|| a.provider.cmp(&b.provider)));

    let mut hasher = blake3::Hasher::new();
    hasher.update(FINGERPRINT_DOMAIN);
    hasher.update(&(sorted.len() as u64).to_le_bytes());
    for o in sorted {
        push_bytes_h(&mut hasher, o.uri.as_bytes());
        push_bytes_h(&mut hasher, o.provider.as_bytes());
        let mut vb = Vec::new();
        o.version.fingerprint_into(&mut vb);
        push_bytes_h(&mut hasher, &vb);
    }
    hex::encode(hasher.finalize().as_bytes())
}

fn push_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
    buf.extend_from_slice(b);
}

fn push_bytes_h(hasher: &mut blake3::Hasher, b: &[u8]) {
    hasher.update(&(b.len() as u64).to_le_bytes());
    hasher.update(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(uri: &str, ver: ObjectVersion) -> PinnedObject {
        PinnedObject {
            uri: uri.into(),
            provider: "s3".into(),
            version: ver,
        }
    }

    fn s3(version_id: &str) -> ObjectVersion {
        ObjectVersion::S3 {
            version_id: Some(version_id.into()),
            etag: Some("etag".into()),
            size: 100,
        }
    }

    #[test]
    fn fingerprint_is_order_independent() {
        let a = vec![obj("s3://b/2.parquet", s3("v2")), obj("s3://b/1.parquet", s3("v1"))];
        let b = vec![obj("s3://b/1.parquet", s3("v1")), obj("s3://b/2.parquet", s3("v2"))];
        assert_eq!(compute_fingerprint(&a), compute_fingerprint(&b));
    }

    #[test]
    fn fingerprint_changes_when_a_version_changes() {
        let base = vec![obj("s3://b/1.parquet", s3("v1"))];
        let changed = vec![obj("s3://b/1.parquet", s3("v2"))];
        assert_ne!(compute_fingerprint(&base), compute_fingerprint(&changed));
    }

    #[test]
    fn fingerprint_changes_when_an_object_is_added() {
        let one = vec![obj("s3://b/1.parquet", s3("v1"))];
        let two = vec![obj("s3://b/1.parquet", s3("v1")), obj("s3://b/2.parquet", s3("v2"))];
        assert_ne!(compute_fingerprint(&one), compute_fingerprint(&two));
    }

    #[test]
    fn from_objects_sorts_and_fingerprints() {
        let snap = InputSnapshot::from_objects(vec![
            obj("s3://b/2.parquet", s3("v2")),
            obj("s3://b/1.parquet", s3("v1")),
        ]);
        assert_eq!(snap.objects[0].uri, "s3://b/1.parquet");
        assert_eq!(snap.fingerprint, compute_fingerprint(&snap.objects));
        assert!(snap.fully_concrete());
    }

    #[test]
    fn distinct_version_variants_do_not_collide() {
        let s3o = vec![obj("u", ObjectVersion::S3 { version_id: None, etag: None, size: 0 })];
        let gcs = vec![obj("u", ObjectVersion::Gcs { generation: None })];
        assert_ne!(compute_fingerprint(&s3o), compute_fingerprint(&gcs));
    }

    #[test]
    fn best_effort_pin_is_not_concrete() {
        let v = ObjectVersion::S3 {
            version_id: None,
            etag: Some("e".into()),
            size: 1,
        };
        assert!(!v.is_concrete());
        assert!(ObjectVersion::ContentAddressed { sha256: "ab".into() }.is_concrete());
    }
}
