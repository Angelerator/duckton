//! Signing & verification of the non-GDPR [`SystemProfile`] (analytics/routing
//! HINT only).
//!
//! Mirrors [`crate::capability::sign_capability_profile`]: the proto
//! [`SystemProfile`] carries the data; here we bind it to a node identity with
//! an Ed25519 signature over canonical, length-prefixed signing bytes, and check
//! the `node_id ↔ pubkey` binding on verify.
//!
//! IMPORTANT (trust boundary): a valid signature only proves the profile was
//! *issued by* the holder of `pubkey`. The contents are **self-reported** and
//! MUST NOT feed trust/selection scoring — they are a routing/analytics hint, the
//! same posture as a self-reported capability ad.

use ed25519_dalek::{Signature, VerifyingKey};
use p2p_proto::{NodeId, SystemProfile};

use crate::receipt::Signer;

const SYSTEM_DOMAIN: &[u8] = b"duckdb-p2p-system-profile-v1";

/// Canonical bytes a [`SystemProfile`]'s signature covers. Stable field order
/// with length-prefixing so distinct field values can never collide.
fn system_signing_bytes(p: &SystemProfile) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u64).to_le_bytes());
        buf.extend_from_slice(b);
    };
    field(SYSTEM_DOMAIN);
    field(&p.schema_version.to_le_bytes());
    field(p.node_id.0.as_bytes());
    field(p.pubkey.as_bytes());
    field(&p.collected_at.to_le_bytes());
    field(&p.refresh_seq.to_le_bytes());
    // CPU
    field(p.cpu_arch.as_bytes());
    field(p.cpu_model.as_bytes());
    field(&p.cpu_physical_cores.to_le_bytes());
    field(&p.cpu_logical_cores.to_le_bytes());
    field(&opt_u64(p.cpu_base_freq_mhz));
    field(&opt_u64(p.cpu_max_freq_mhz));
    field(&(p.cpu_features.len() as u64).to_le_bytes());
    for f in &p.cpu_features {
        field(f.as_bytes());
    }
    // Memory
    field(&p.ram_total_bytes.to_le_bytes());
    field(&p.ram_available_bytes.to_le_bytes());
    field(&p.swap_total_bytes.to_le_bytes());
    field(&p.swap_used_bytes.to_le_bytes());
    // Disk
    field(&p.disk_total_bytes.to_le_bytes());
    field(&p.disk_available_bytes.to_le_bytes());
    field(p.disk_kind.as_bytes());
    // OS
    field(p.os_name.as_bytes());
    field(p.os_version.as_bytes());
    field(p.kernel_version.as_bytes());
    field(p.virt_hint.as_bytes());
    field(&p.numa_nodes.to_le_bytes());
    // Build versions
    field(p.engine_version.as_bytes());
    field(p.extension_version.as_bytes());
    // Resource ceilings
    field(&opt_u64(p.process_rlimit_as_bytes));
    field(&opt_u64(p.cgroup_memory_max_bytes));
    field(&opt_f64(p.cgroup_cpu_quota));
    // Donated budget
    field(&p.donated_budget_mem_bytes.to_le_bytes());
    field(&p.donated_budget_threads.to_le_bytes());
    buf
}

/// Encode `Option<u64>` as a presence byte + 8 LE bytes (so `None` and
/// `Some(0)` produce distinct signing bytes).
fn opt_u64(v: Option<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    match v {
        Some(x) => {
            out.push(1);
            out.extend_from_slice(&x.to_le_bytes());
        }
        None => out.push(0),
    }
    out
}

/// Encode `Option<f64>` as a presence byte + 8 LE bytes of the IEEE-754 bits.
fn opt_f64(v: Option<f64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    match v {
        Some(x) => {
            out.push(1);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        None => out.push(0),
    }
    out
}

/// Sign a [`SystemProfile`] with the node identity. Sets `node_id`/`pubkey` from
/// the signer (so a profile is always bound to the signer) and fills `sig`.
pub fn sign_system_profile(mut p: SystemProfile, signer: &impl Signer) -> SystemProfile {
    p.node_id = signer.node_id();
    p.pubkey = hex::encode(signer.public_key());
    p.sig = String::new();
    let sig = signer.sign_bytes(&system_signing_bytes(&p));
    p.sig = hex::encode(sig);
    p
}

/// Verify a [`SystemProfile`]: signature + `node_id ↔ pubkey` binding. Returns
/// `false` for a tampered field, a wrong-node copy, or a malformed signature.
pub fn verify_system_profile(p: &SystemProfile) -> bool {
    let pk_bytes = match hex::decode(&p.pubkey) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let pk: [u8; 32] = match pk_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    if p.node_id != NodeId::from_pubkey(&pk) {
        return false;
    }
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(k) => k,
        Err(_) => return false,
    };
    let sig_bytes = match hex::decode(&p.sig) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let sig_arr: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    vk.verify_strict(&system_signing_bytes(p), &Signature::from_bytes(&sig_arr))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use p2p_proto::SystemProfile;
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

    fn profile() -> SystemProfile {
        let mut p = SystemProfile::empty(NodeId("placeholder".into()), String::new());
        p.cpu_arch = "aarch64".into();
        p.cpu_model = "Apple M2".into();
        p.cpu_physical_cores = 8;
        p.cpu_logical_cores = 8;
        p.cpu_features = vec!["neon".into()];
        p.ram_total_bytes = 16 * 1024 * 1024 * 1024;
        p.cgroup_cpu_quota = Some(2.5);
        p.donated_budget_mem_bytes = 1 << 30;
        p.donated_budget_threads = 4;
        p
    }

    #[test]
    fn sign_and_verify_system_profile() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let p = sign_system_profile(profile(), &signer);
        // Signing bound the profile to the signer's identity.
        assert_eq!(p.node_id, signer.node_id());
        assert!(verify_system_profile(&p));
    }

    #[test]
    fn tampered_profile_rejected() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut p = sign_system_profile(profile(), &signer);
        p.ram_total_bytes = u64::MAX; // inflate after signing
        assert!(!verify_system_profile(&p));
    }

    #[test]
    fn tampered_optional_field_rejected() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let mut p = sign_system_profile(profile(), &signer);
        // Flip an Option from Some -> None: presence-byte encoding makes the
        // signing bytes differ, so verification must fail.
        p.cgroup_cpu_quota = None;
        assert!(!verify_system_profile(&p));
    }

    #[test]
    fn profile_from_another_node_rejected() {
        let a = TestSigner(SigningKey::generate(&mut OsRng));
        let b = TestSigner(SigningKey::generate(&mut OsRng));
        let mut p = sign_system_profile(profile(), &a);
        p.node_id = b.node_id(); // re-label as B without B's signature
        assert!(!verify_system_profile(&p));
    }
}
