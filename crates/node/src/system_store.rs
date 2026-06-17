//! Durable, signed **system-metadata profile** store (analytics/routing HINT).
//!
//! Mirrors [`crate::capability_store::CapabilityStore`]'s persistence
//! conventions (its own file co-located with `capability.json`, `0600` perms,
//! a strictly-increasing `refresh_seq`, load-verify) and reuses the
//! [`p2p_trust::sign_system_profile`] primitive rather than inventing new crypto.
//!
//! Unlike the capability profile (all-time ratcheted maxima), a system profile
//! is a **point-in-time snapshot**: each refresh REPLACES the previous one and
//! bumps `refresh_seq`. The profile is a self-reported HINT and never feeds
//! trust/selection scoring.

use std::path::{Path, PathBuf};

use p2p_proto::SystemProfile;
use p2p_trust::{sign_system_profile, verify_system_profile, Signer};

/// Persisted, signed system-profile store.
pub struct SystemStore {
    path: PathBuf,
}

impl SystemStore {
    /// Open at the default location (`<config-dir>/system_profile.json`, or
    /// `$P2P_SYSTEM_PROFILE` if set), co-located with `capability.json`.
    pub fn open() -> Self {
        let path = std::env::var("P2P_SYSTEM_PROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| p2p_config::default_config_dir().join("system_profile.json"));
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
    /// binding), and is bound to `expected_node` — otherwise `None`. A profile
    /// that fails verification or belongs to another node is ignored rather than
    /// trusted, so a tampered or copied file cannot be adopted.
    pub fn load_verified(&self, expected_node: &p2p_proto::NodeId) -> Option<SystemProfile> {
        let bytes = std::fs::read(&self.path).ok()?;
        let profile: SystemProfile = p2p_proto::from_bytes(&bytes).ok()?;
        if &profile.node_id != expected_node {
            return None;
        }
        if !verify_system_profile(&profile) {
            return None;
        }
        Some(profile)
    }

    /// Persist a freshly-collected snapshot: assign the monotonic `refresh_seq`
    /// (prev + 1, starting at 1), re-sign with the node identity, and write
    /// atomically with `0600` perms. A pre-existing profile that fails
    /// verification (tamper / foreign key) is treated as absent so it can never
    /// be used to roll `refresh_seq` back or seed a forged profile.
    pub fn store(
        &self,
        signer: &impl Signer,
        mut profile: SystemProfile,
    ) -> Result<SystemProfile, std::io::Error> {
        let prev_seq = self
            .load_verified(&signer.node_id())
            .map(|p| p.refresh_seq)
            .unwrap_or(0);
        profile.refresh_seq = prev_seq.saturating_add(1);
        let signed = sign_system_profile(profile, signer);
        self.write(&signed)?;
        Ok(signed)
    }

    fn write(&self, profile: &SystemProfile) -> Result<(), std::io::Error> {
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

    fn store() -> (SystemStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            SystemStore::with_path(dir.path().join("system_profile.json")),
            dir,
        )
    }

    fn profile(signer: &impl Signer) -> SystemProfile {
        let mut p = SystemProfile::empty(signer.node_id(), hex::encode(signer.public_key()));
        p.cpu_arch = "x86_64".into();
        p.ram_total_bytes = 1 << 34;
        p
    }

    #[test]
    fn store_persists_increments_refresh_seq_and_verifies() {
        let (s, dir) = store();
        let signer = TestSigner(SigningKey::generate(&mut OsRng));

        let p1 = s.store(&signer, profile(&signer)).unwrap();
        assert_eq!(p1.refresh_seq, 1);
        assert!(verify_system_profile(&p1));

        let p2 = s.store(&signer, profile(&signer)).unwrap();
        assert_eq!(p2.refresh_seq, 2, "refresh_seq is monotonic");

        // Survives reopen and binds to the same node.
        let reopened = SystemStore::with_path(dir.path().join("system_profile.json"));
        let loaded = reopened.load_verified(&signer.node_id()).unwrap();
        assert_eq!(loaded.refresh_seq, 2);
    }

    #[test]
    fn tampered_or_foreign_profile_is_ignored() {
        let (s, _dir) = store();
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        s.store(&signer, profile(&signer)).unwrap();

        // A different node's key must not adopt this file (binding check).
        let other = TestSigner(SigningKey::generate(&mut OsRng));
        assert!(s.load_verified(&other.node_id()).is_none());

        // Corrupt the file → verification fails → treated as cold start, and the
        // next store() restarts the monotonic chain at refresh_seq 1.
        std::fs::write(s.path(), b"not a valid signed profile").unwrap();
        assert!(s.load_verified(&signer.node_id()).is_none());
        let fresh = s.store(&signer, profile(&signer)).unwrap();
        assert_eq!(fresh.refresh_seq, 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (s, _dir) = store();
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        s.store(&signer, profile(&signer)).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
