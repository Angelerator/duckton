//! Non-GDPR **system-metadata profile** (analytics / routing HINT only).
//!
//! A [`SystemProfile`] is a compact, signed snapshot of the **machine class** a
//! node runs on — CPU/RAM/disk/OS shape, the donated compute budget, and the
//! host's resource ceilings (rlimit / cgroup). It is keyed by the pseudonymous
//! Ed25519 `node_id`, **never** a person or host identity.
//!
//! ## Trust boundary (must hold)
//! This is **self-reported** and therefore a routing/analytics hint *only*. It
//! MUST NOT feed trust or selection scoring — that stays receipt-driven (see
//! `capability_confidence`). A node could lie here; nothing of value depends on
//! it being true, exactly like the self-reported [`crate::CapabilityAd`].
//!
//! ## GDPR / PII exclusions (hard requirement)
//! The profile deliberately omits everything that could identify a person or a
//! specific host: NO hostname/FQDN, usernames/UID names, IP/MAC addresses,
//! machine-id / DMI serials / hardware UUIDs, precise geolocation, wallet/keys,
//! environment variables, or file paths. Only machine-CLASS shape is captured.
//! `crates/proto/src/system.rs`'s `no_pii_in_serialized_profile` test guards this.
//!
//! The proto layer only carries the data + the signature; the `p2p-trust` layer
//! signs and verifies it (it owns the crypto), mirroring [`crate::capability`].

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;

/// Wire schema tag for [`SystemProfile`] records (bumped on a breaking change).
pub const SYSTEM_PROFILE_SCHEMA_VERSION: u16 = 1;

/// A node's signed, non-GDPR **system-metadata profile** (machine class). See
/// the module docs for the trust boundary + the PII exclusions.
///
/// `PartialEq` only (not `Eq`): `cgroup_cpu_quota` is an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemProfile {
    /// Wire schema tag of this record.
    pub schema_version: u16,
    /// The pseudonymous node identity this profile describes (NOT a host name).
    pub node_id: NodeId,
    /// Hex Ed25519 public key (so verifiers can derive node_id and check `sig`).
    pub pubkey: String,
    /// Unix-seconds timestamp this snapshot was collected.
    pub collected_at: u64,
    /// Strictly-increasing refresh counter (rollback/monotonicity guard).
    pub refresh_seq: u64,

    // --- CPU (machine class — no serial / identifier) ----------------------
    /// CPU architecture (e.g. `x86_64`, `aarch64`).
    pub cpu_arch: String,
    /// CPU model/brand string (e.g. `Apple M2`, `AMD EPYC 7763`).
    pub cpu_model: String,
    /// Physical core count (`0` when undetectable).
    pub cpu_physical_cores: u32,
    /// Logical core count (hardware threads).
    pub cpu_logical_cores: u32,
    /// Base/nominal frequency in MHz, when known.
    pub cpu_base_freq_mhz: Option<u64>,
    /// Max frequency in MHz, when known.
    pub cpu_max_freq_mhz: Option<u64>,
    /// Detected ISA feature flags (e.g. `avx2`, `neon`) — best-effort.
    pub cpu_features: Vec<String>,

    // --- Memory ------------------------------------------------------------
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,

    // --- Disk (aggregate machine class — no mount paths / device names) -----
    pub disk_total_bytes: u64,
    pub disk_available_bytes: u64,
    /// `ssd` | `hdd` | `nvme` | `unknown` (best-effort).
    pub disk_kind: String,

    // --- OS (no hostname) --------------------------------------------------
    pub os_name: String,
    pub os_version: String,
    pub kernel_version: String,
    /// Virtualization/container hint (`docker` | `container` | `vm` | `unknown`).
    pub virt_hint: String,
    /// NUMA node count (`1` when undetectable).
    pub numa_nodes: u32,

    // --- Build versions ----------------------------------------------------
    pub engine_version: String,
    pub extension_version: String,

    // --- Resource ceilings (so analytics can tell the EFFECTIVE budget apart
    //     from the donated one) ----------------------------------------------
    /// `RLIMIT_AS` address-space ceiling in bytes (`None` = unlimited/unknown).
    pub process_rlimit_as_bytes: Option<u64>,
    /// cgroup v2 `memory.max` in bytes (`None` = `max`/unknown — Linux only).
    pub cgroup_memory_max_bytes: Option<u64>,
    /// cgroup v2 `cpu.max` as a CPU count (`quota/period`; `None` = `max`/unknown).
    pub cgroup_cpu_quota: Option<f64>,

    // --- Donated compute budget (what this node OFFERS the grid, distinct from
    //     the physical RAM/threads above) -------------------------------------
    pub donated_budget_mem_bytes: u64,
    pub donated_budget_threads: u32,

    /// Hex Ed25519 signature over the canonical signing bytes (`p2p-trust`).
    pub sig: String,
}

impl SystemProfile {
    /// An empty, unsigned profile bound to `node_id`/`pubkey` (the cold-start
    /// base the collector fills in). All measured fields default to zero/unknown;
    /// `p2p_trust::sign_system_profile` fills `sig`.
    pub fn empty(node_id: NodeId, pubkey: String) -> Self {
        Self {
            schema_version: SYSTEM_PROFILE_SCHEMA_VERSION,
            node_id,
            pubkey,
            collected_at: 0,
            refresh_seq: 0,
            cpu_arch: String::new(),
            cpu_model: String::new(),
            cpu_physical_cores: 0,
            cpu_logical_cores: 0,
            cpu_base_freq_mhz: None,
            cpu_max_freq_mhz: None,
            cpu_features: Vec::new(),
            ram_total_bytes: 0,
            ram_available_bytes: 0,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            disk_total_bytes: 0,
            disk_available_bytes: 0,
            disk_kind: "unknown".into(),
            os_name: String::new(),
            os_version: String::new(),
            kernel_version: String::new(),
            virt_hint: "unknown".into(),
            numa_nodes: 1,
            engine_version: String::new(),
            extension_version: String::new(),
            process_rlimit_as_bytes: None,
            cgroup_memory_max_bytes: None,
            cgroup_cpu_quota: None,
            donated_budget_mem_bytes: 0,
            donated_budget_threads: 0,
            sig: String::new(),
        }
    }

    /// Render the profile as flat `(group, key, value)` rows for the SQL
    /// `p2p_node_metadata()` surface (analytics). Excludes the signature/pubkey
    /// (cryptographic plumbing, not user-facing metadata).
    pub fn metadata_rows(&self) -> Vec<[String; 3]> {
        let opt_u = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "unknown".into());
        let opt_f = |v: Option<f64>| v.map(|x| x.to_string()).unwrap_or_else(|| "unknown".into());
        let row = |g: &str, k: &str, v: String| [g.to_string(), k.to_string(), v];
        vec![
            row("identity", "node_id", self.node_id.0.clone()),
            row(
                "identity",
                "schema_version",
                self.schema_version.to_string(),
            ),
            row("identity", "collected_at", self.collected_at.to_string()),
            row("identity", "refresh_seq", self.refresh_seq.to_string()),
            row("cpu", "arch", self.cpu_arch.clone()),
            row("cpu", "model", self.cpu_model.clone()),
            row("cpu", "physical_cores", self.cpu_physical_cores.to_string()),
            row("cpu", "logical_cores", self.cpu_logical_cores.to_string()),
            row("cpu", "base_freq_mhz", opt_u(self.cpu_base_freq_mhz)),
            row("cpu", "max_freq_mhz", opt_u(self.cpu_max_freq_mhz)),
            row("cpu", "features", self.cpu_features.join(",")),
            row(
                "memory",
                "ram_total_bytes",
                self.ram_total_bytes.to_string(),
            ),
            row(
                "memory",
                "ram_available_bytes",
                self.ram_available_bytes.to_string(),
            ),
            row(
                "memory",
                "swap_total_bytes",
                self.swap_total_bytes.to_string(),
            ),
            row(
                "memory",
                "swap_used_bytes",
                self.swap_used_bytes.to_string(),
            ),
            row("disk", "total_bytes", self.disk_total_bytes.to_string()),
            row(
                "disk",
                "available_bytes",
                self.disk_available_bytes.to_string(),
            ),
            row("disk", "kind", self.disk_kind.clone()),
            row("os", "name", self.os_name.clone()),
            row("os", "version", self.os_version.clone()),
            row("os", "kernel_version", self.kernel_version.clone()),
            row("os", "virt_hint", self.virt_hint.clone()),
            row("os", "numa_nodes", self.numa_nodes.to_string()),
            row("build", "engine_version", self.engine_version.clone()),
            row("build", "extension_version", self.extension_version.clone()),
            row(
                "limits",
                "process_rlimit_as_bytes",
                opt_u(self.process_rlimit_as_bytes),
            ),
            row(
                "limits",
                "cgroup_memory_max_bytes",
                opt_u(self.cgroup_memory_max_bytes),
            ),
            row("limits", "cgroup_cpu_quota", opt_f(self.cgroup_cpu_quota)),
            row(
                "budget",
                "donated_mem_bytes",
                self.donated_budget_mem_bytes.to_string(),
            ),
            row(
                "budget",
                "donated_threads",
                self.donated_budget_threads.to_string(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SystemProfile {
        let mut p = SystemProfile::empty(NodeId("b3:w".into()), "00".repeat(32));
        p.cpu_arch = "x86_64".into();
        p.cpu_model = "AMD EPYC 7763".into();
        p.cpu_physical_cores = 64;
        p.cpu_logical_cores = 128;
        p.cpu_features = vec!["avx2".into(), "sse4.2".into()];
        p.ram_total_bytes = 256 * 1024 * 1024 * 1024;
        p.disk_kind = "ssd".into();
        p.os_name = "Linux".into();
        p.cgroup_cpu_quota = Some(4.0);
        p.donated_budget_mem_bytes = 4 * 1024 * 1024 * 1024;
        p.donated_budget_threads = 8;
        p
    }

    #[test]
    fn system_profile_roundtrips() {
        let p = sample();
        let bytes = crate::to_bytes(&p).unwrap();
        let back: SystemProfile = crate::from_bytes(&bytes).unwrap();
        assert_eq!(p, back);
    }

    /// GDPR/PII guard: the serialized profile must contain no hostname/FQDN,
    /// IP/MAC, or username patterns. We populate the realistic machine-class
    /// fields and assert none of the forbidden identity markers appear.
    #[test]
    fn no_pii_in_serialized_profile() {
        let mut p = sample();
        // OS/version fields are the most likely accidental PII carriers — fill
        // them with realistic, NON-identifying machine-class values.
        p.os_version = "5.15.0".into();
        p.kernel_version = "5.15.0-91-generic".into();
        let json = serde_json::to_string(&p).unwrap();

        // No PII *field keys* may exist (the struct simply has no such fields).
        for forbidden_key in [
            "hostname",
            "fqdn",
            "host_name",
            "username",
            "user_name",
            "uid",
            "ip_addr",
            "ip_address",
            "mac_addr",
            "mac_address",
            "machine_id",
            "serial",
            "dmi",
            "hardware_uuid",
            "geolocation",
            "latitude",
            "longitude",
            "env",
            "home_dir",
            "wallet",
            "private_key",
        ] {
            assert!(
                !json.contains(forbidden_key),
                "serialized SystemProfile must not contain PII key `{forbidden_key}`: {json}"
            );
        }

        // No literal IPv4/IPv6/MAC value shapes (defense in depth against a value
        // accidentally carrying an address). The realistic kernel version
        // `5.15.0-91` is dotted but NOT a 4-octet IPv4, so it passes.
        let looks_like_ipv4 = json.split(['"', ',', ' ', ':']).any(|tok| {
            let parts: Vec<&str> = tok.split('.').collect();
            parts.len() == 4
                && parts
                    .iter()
                    .all(|s| !s.is_empty() && s.parse::<u8>().is_ok())
        });
        assert!(
            !looks_like_ipv4,
            "serialized profile looks like it carries an IPv4: {json}"
        );
        // A MAC address shape `xx:xx:xx:xx:xx:xx`.
        let looks_like_mac = json.split('"').any(|tok| {
            let parts: Vec<&str> = tok.split(':').collect();
            parts.len() == 6
                && parts
                    .iter()
                    .all(|s| s.len() == 2 && u8::from_str_radix(s, 16).is_ok())
        });
        assert!(
            !looks_like_mac,
            "serialized profile looks like it carries a MAC: {json}"
        );
    }

    #[test]
    fn metadata_rows_exclude_signature_and_cover_groups() {
        let p = sample();
        let rows = p.metadata_rows();
        let groups: std::collections::BTreeSet<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        for g in [
            "cpu", "memory", "disk", "os", "build", "limits", "budget", "identity",
        ] {
            assert!(groups.contains(g), "missing metadata group {g}");
        }
        // The signature/pubkey are crypto plumbing, never surfaced as metadata.
        assert!(rows.iter().all(|r| r[1] != "sig" && r[1] != "pubkey"));
    }
}
