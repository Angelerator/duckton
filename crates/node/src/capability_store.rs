//! Durable, tamper-resistant **self-measured capability profile** store
//! (routing design §3). A node keeps a compact, all-time, monotonic record of
//! the largest real workloads it has actually completed locally — persisted
//! across restarts, Ed25519-signed + node-id-bound, and rollback-guarded by a
//! strictly-increasing `seq`.
//!
//! This mirrors [`p2p_config::BlocklistStore`]'s persistence conventions (its
//! own file, friendly errors, never panics on bad input) and reuses the
//! existing [`p2p_trust::sign_capability_profile`] signing primitive — the same
//! pattern that signs gossiped capability ads — rather than inventing new
//! crypto. The profile is bounded by construction (a fixed set of maxima +
//! counters, never a per-job log).

use std::path::{Path, PathBuf};

use p2p_proto::CapabilityProfile;
use p2p_trust::{
    now_ts, sign_capability_profile, verify_capability_profile, CapabilityProfileDraft, Signer,
};

/// One real, successful local execution's MEASURED magnitude (from DuckDB
/// profiling / the delivered result). Folded into the profile's maxima.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeasuredExecution {
    pub input_bytes: u64,
    pub result_rows: u64,
    pub result_bytes: u64,
    /// Peak buffer memory the run reached (e.g. `peak_buffer_memory`).
    pub peak_memory_bytes: u64,
    /// Peak temp-dir (spill) the run reached (e.g. `peak_temp_dir_size`) — proves
    /// out-of-core capability above RAM.
    pub temp_dir_bytes: u64,
}

/// Persisted, signed capability-profile store.
pub struct CapabilityStore {
    path: PathBuf,
}

impl CapabilityStore {
    /// Open at the default location (`<config-dir>/capability.json`, or
    /// `$P2P_CAPABILITY` if set), co-located with `runtime.toml`/`blocklist.toml`.
    pub fn open() -> Self {
        let path = std::env::var("P2P_CAPABILITY")
            .map(PathBuf::from)
            .unwrap_or_else(|_| p2p_config::default_config_dir().join("capability.json"));
        Self { path }
    }

    /// Construct with an explicit path (tests).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the persisted profile if it exists, verifies (signature + node-id
    /// binding), and is bound to `expected_node` — otherwise `None` (cold start).
    /// A profile that fails verification or belongs to another node is ignored
    /// rather than trusted, so a tampered or copied file cannot be adopted.
    pub fn load_verified(&self, expected_node: &p2p_proto::NodeId) -> Option<CapabilityProfile> {
        let bytes = std::fs::read(&self.path).ok()?;
        let profile: CapabilityProfile = p2p_proto::from_bytes(&bytes).ok()?;
        if &profile.node_id != expected_node {
            return None;
        }
        if !verify_capability_profile(&profile) {
            return None;
        }
        Some(profile)
    }

    /// Fold a measured successful execution into the profile: ratchet every
    /// maximum upward (never down), increment `successes` + the monotonic `seq`,
    /// re-sign with the node identity, and persist atomically. Returns the new
    /// signed profile. A pre-existing profile that fails verification (tamper /
    /// foreign key) is treated as absent (cold start) so it can never be used to
    /// silently downgrade or seed a forged capability.
    pub fn observe(
        &self,
        signer: &impl Signer,
        m: MeasuredExecution,
    ) -> Result<CapabilityProfile, std::io::Error> {
        let prev = self.load_verified(&signer.node_id());
        let base = prev.unwrap_or(CapabilityProfile {
            schema_version: 0,
            node_id: signer.node_id(),
            pubkey: String::new(),
            max_input_bytes: 0,
            max_result_rows: 0,
            max_result_bytes: 0,
            max_peak_memory_bytes: 0,
            max_temp_dir_bytes: 0,
            successes: 0,
            seq: 0,
            ts: 0,
            sig: String::new(),
        });

        let draft = CapabilityProfileDraft {
            max_input_bytes: base.max_input_bytes.max(m.input_bytes),
            max_result_rows: base.max_result_rows.max(m.result_rows),
            max_result_bytes: base.max_result_bytes.max(m.result_bytes),
            max_peak_memory_bytes: base.max_peak_memory_bytes.max(m.peak_memory_bytes),
            max_temp_dir_bytes: base.max_temp_dir_bytes.max(m.temp_dir_bytes),
            successes: base.successes.saturating_add(1),
            // Strictly increasing: the rollback/monotonicity guard.
            seq: base.seq.saturating_add(1),
            ts: now_ts(),
        };
        let profile = sign_capability_profile(draft, signer);
        self.write(&profile)?;
        Ok(profile)
    }

    fn write(&self, profile: &CapabilityProfile) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = p2p_proto::to_bytes(profile)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(&self.path, bytes)?;
        restrict_permissions(&self.path);
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(path: &Path) {
    // Windows: install an owner-only protected DACL (shared with the config
    // store's secret hardening). No-op on other non-Unix targets.
    p2p_config::restrict_path_to_owner(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use p2p_proto::NodeId;
    use rand::rngs::OsRng;

    struct TestSigner(SigningKey);
    impl Signer for TestSigner {
        fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
            self.0.sign(msg).to_bytes()
        }
        fn public_key(&self) -> [u8; 32] {
            self.0.verifying_key().to_bytes()
        }
        fn node_id(&self) -> NodeId {
            NodeId::from_pubkey(&self.0.verifying_key().to_bytes())
        }
    }

    fn store() -> (CapabilityStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            CapabilityStore::with_path(dir.path().join("capability.json")),
            dir,
        )
    }

    #[test]
    fn observe_ratchets_persists_and_increments_seq() {
        let (s, dir) = store();
        let signer = TestSigner(SigningKey::generate(&mut OsRng));

        let p1 = s
            .observe(
                &signer,
                MeasuredExecution {
                    input_bytes: 1_000,
                    result_rows: 50,
                    result_bytes: 2_000,
                    peak_memory_bytes: 4_000,
                    temp_dir_bytes: 8_000,
                },
            )
            .unwrap();
        assert_eq!(p1.seq, 1);
        assert_eq!(p1.successes, 1);
        assert_eq!(p1.max_result_rows, 50);
        assert!(verify_capability_profile(&p1));

        // A smaller later run bumps successes/seq but never lowers the maxima.
        let p2 = s
            .observe(
                &signer,
                MeasuredExecution {
                    result_rows: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(p2.seq, 2);
        assert_eq!(p2.successes, 2);
        assert_eq!(p2.max_result_rows, 50, "maxima ratchet up only");
        assert_eq!(p2.max_temp_dir_bytes, 8_000);

        // Survives reopen and verifies + binds to the same node.
        let reopened = CapabilityStore::with_path(dir.path().join("capability.json"));
        let loaded = reopened.load_verified(&signer.node_id()).unwrap();
        assert_eq!(loaded.seq, 2);
        assert_eq!(loaded.max_result_rows, 50);
    }

    #[test]
    fn tampered_or_foreign_profile_is_ignored() {
        let (s, _dir) = store();
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        s.observe(&signer, MeasuredExecution::default()).unwrap();

        // A different node's key must not adopt this file (binding check).
        let other = TestSigner(SigningKey::generate(&mut OsRng));
        assert!(s.load_verified(&other.node_id()).is_none());

        // Corrupt the file on disk → verification fails → treated as cold start.
        std::fs::write(s.path(), b"not a valid signed profile").unwrap();
        assert!(s.load_verified(&signer.node_id()).is_none());
        // observe() then starts a fresh monotonic chain at seq 1 rather than
        // trusting the corrupt bytes.
        let fresh = s.observe(&signer, MeasuredExecution::default()).unwrap();
        assert_eq!(fresh.seq, 1);
    }
}
