//! Non-GDPR **system-metadata collection** (analytics / routing HINT only).
//!
//! [`collect_system_profile`] probes the host's machine CLASS — CPU/RAM/disk/OS
//! shape + resource ceilings — via [`sysinfo`] plus small `#[cfg]` helpers for
//! the gaps sysinfo doesn't cover (ISA features, cgroup v2 limits, rlimit, virt
//! detection, NUMA). It produces a [`SystemProfile`] bound to the node identity.
//!
//! ## Trust boundary
//! Everything here is **self-reported**. The result is a routing/analytics hint
//! only and MUST NOT feed trust/selection scoring (that stays receipt-driven).
//!
//! ## GDPR / PII exclusions (hard requirement)
//! We deliberately capture only machine-CLASS shape. NO hostname/FQDN, no
//! usernames/UIDs, no IP/MAC, no machine-id / DMI serials / hardware UUIDs, no
//! geolocation, no env vars, and no file paths. Where a probe reads a system
//! file (e.g. `/proc/1/cgroup`) we only substring-match it for a class hint and
//! never store its contents. `crates/proto/src/system.rs`'s
//! `no_pii_in_serialized_profile` test guards the serialized shape.

use p2p_config::BudgetConfig;
use p2p_proto::SystemProfile;
use p2p_trust::{now_ts, Signer};
use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};

/// Collect a fresh, UNSIGNED [`SystemProfile`] for this host bound to `signer`'s
/// identity. `refresh_seq` is left at `0`; the [`crate::system_store::SystemStore`]
/// assigns the monotonic value and signs. Call sites then sign via
/// [`p2p_trust::sign_system_profile`].
pub fn collect_system_profile(
    signer: &impl Signer,
    budget: &BudgetConfig,
    engine_version: &str,
    extension_version: &str,
) -> SystemProfile {
    let mut profile =
        SystemProfile::empty(signer.node_id(), hex::encode(signer.public_key()));
    profile.collected_at = now_ts();

    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::everything())
            .with_cpu(CpuRefreshKind::everything()),
    );

    // --- CPU (no serial / identifier) --------------------------------------
    profile.cpu_arch = System::cpu_arch();
    let cpus = sys.cpus();
    profile.cpu_logical_cores = cpus.len() as u32;
    profile.cpu_physical_cores = System::physical_core_count().unwrap_or(0) as u32;
    if let Some(c0) = cpus.first() {
        profile.cpu_model = c0.brand().trim().to_string();
        let freq = c0.frequency();
        // sysinfo reports the *current* per-core MHz; use it as a best-effort
        // base when non-zero (documented approximation — not the nominal base).
        profile.cpu_base_freq_mhz = (freq > 0).then_some(freq);
    }
    profile.cpu_max_freq_mhz = cpu_max_freq_mhz();
    profile.cpu_features = cpu_features();

    // --- Memory ------------------------------------------------------------
    profile.ram_total_bytes = sys.total_memory();
    profile.ram_available_bytes = sys.available_memory();
    profile.swap_total_bytes = sys.total_swap();
    profile.swap_used_bytes = sys.used_swap();

    // --- Disk (representative primary disk; no mount paths / device names) ---
    let (disk_total, disk_avail, disk_kind) = disk_summary();
    profile.disk_total_bytes = disk_total;
    profile.disk_available_bytes = disk_avail;
    profile.disk_kind = disk_kind;

    // --- OS (no hostname) --------------------------------------------------
    profile.os_name = System::name().unwrap_or_default();
    profile.os_version = System::os_version().unwrap_or_default();
    profile.kernel_version = System::kernel_version().unwrap_or_default();
    profile.virt_hint = virt_hint();
    profile.numa_nodes = numa_nodes();

    // --- Build versions ----------------------------------------------------
    profile.engine_version = engine_version.to_string();
    profile.extension_version = extension_version.to_string();

    // --- Resource ceilings -------------------------------------------------
    profile.process_rlimit_as_bytes = rlimit_as_bytes();
    let (cg_mem, cg_cpu) = cgroup_limits();
    profile.cgroup_memory_max_bytes = cg_mem;
    profile.cgroup_cpu_quota = cg_cpu;

    // --- Donated compute budget (what this node OFFERS, distinct from RAM) ---
    profile.donated_budget_mem_bytes = budget.memory_bytes;
    profile.donated_budget_threads = budget.threads;

    profile
}

/// Pick a representative primary disk (the one with the largest total space, to
/// avoid summing overlay/tmpfs/virtual filesystems) and return
/// `(total, available, kind)`. `kind` is `ssd` | `hdd` | `nvme` | `unknown`.
fn disk_summary() -> (u64, u64, String) {
    let disks = Disks::new_with_refreshed_list();
    let primary = disks.list().iter().max_by_key(|d| d.total_space());
    match primary {
        Some(d) => {
            let kind = match d.kind() {
                sysinfo::DiskKind::SSD => "ssd",
                sysinfo::DiskKind::HDD => "hdd",
                sysinfo::DiskKind::Unknown(_) => "unknown",
            };
            // Prefer the more specific "nvme" when the host clearly has NVMe
            // block devices and the kind isn't an explicit HDD (best-effort,
            // Linux-only; reads only the generic `/sys/block` class, no paths).
            let kind = if kind != "hdd" && has_nvme_block_device() {
                "nvme".to_string()
            } else {
                kind.to_string()
            };
            (d.total_space(), d.available_space(), kind)
        }
        None => (0, 0, "unknown".to_string()),
    }
}

#[cfg(target_os = "linux")]
fn has_nvme_block_device() -> bool {
    std::fs::read_dir("/sys/block")
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("nvme"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn has_nvme_block_device() -> bool {
    false
}

/// Detected ISA feature flags (best-effort, per architecture). Empty on
/// architectures we don't enumerate.
#[cfg(target_arch = "x86_64")]
fn cpu_features() -> Vec<String> {
    // Compile-time `cfg!(target_feature = ...)` (what this binary was built with)
    // rather than the runtime `is_x86_feature_detected!` macro, whose accepted
    // token set varies across toolchains and would fail to compile (it rejects
    // standard tokens like `popcnt`/`sse2` as "unknown x86 target feature" on the
    // registry build's compiler). Best-effort; this field is an analytics hint
    // only and is kept out of trust scoring. Mirrors the aarch64 branch below.
    let mut v = Vec::new();
    macro_rules! feat {
        ($name:literal) => {
            if cfg!(target_feature = $name) {
                v.push($name.to_string());
            }
        };
    }
    feat!("sse2");
    feat!("ssse3");
    feat!("sse4.1");
    feat!("sse4.2");
    feat!("avx");
    feat!("avx2");
    feat!("avx512f");
    feat!("fma");
    feat!("aes");
    feat!("bmi1");
    feat!("bmi2");
    feat!("popcnt");
    v
}

#[cfg(target_arch = "aarch64")]
fn cpu_features() -> Vec<String> {
    // Compile-time `cfg!(target_feature = ...)` (what this binary was built with)
    // rather than the runtime `is_aarch64_feature_detected!` macro, whose accepted
    // token set varies across toolchains and would fail to compile. Best-effort.
    let mut v = Vec::new();
    macro_rules! feat {
        ($name:literal) => {
            if cfg!(target_feature = $name) {
                v.push($name.to_string());
            }
        };
    }
    feat!("neon");
    feat!("aes");
    feat!("sha2");
    feat!("sha3");
    feat!("crc");
    feat!("lse");
    feat!("rcpc");
    feat!("dotprod");
    v
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn cpu_features() -> Vec<String> {
    Vec::new()
}

/// Max CPU frequency in MHz from Linux cpufreq (`cpuinfo_max_freq` is in kHz).
/// `None` elsewhere / when unavailable.
#[cfg(target_os = "linux")]
fn cpu_max_freq_mhz() -> Option<u64> {
    let khz: u64 = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (khz > 0).then_some(khz / 1000)
}

#[cfg(not(target_os = "linux"))]
fn cpu_max_freq_mhz() -> Option<u64> {
    None
}

/// `RLIMIT_AS` (address-space) soft ceiling in bytes; `None` when unlimited or
/// unavailable. `libc` is only linked on unix targets.
#[cfg(unix)]
fn rlimit_as_bytes() -> Option<u64> {
    // SAFETY: `getrlimit` fills a fully-owned, zero-initialized `rlimit`.
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_AS, &mut rl) == 0 && rl.rlim_cur != libc::RLIM_INFINITY {
            Some(rl.rlim_cur as u64)
        } else {
            None
        }
    }
}

#[cfg(not(unix))]
fn rlimit_as_bytes() -> Option<u64> {
    None
}

/// cgroup v2 `(memory.max bytes, cpu.max as CPU count)`. `None`/`None` when not
/// on Linux, unset (`max`), or unreadable. The `cpu.max` file is `"quota period"`
/// in microseconds (or `"max <period>"`); the CPU count is `quota/period`. This
/// mirrors the file format the sandbox layer writes (`sandbox/linux.rs`).
#[cfg(target_os = "linux")]
fn cgroup_limits() -> (Option<u64>, Option<f64>) {
    let mem = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|s| {
            let t = s.trim();
            if t == "max" {
                None
            } else {
                t.parse::<u64>().ok()
            }
        });
    let cpu = std::fs::read_to_string("/sys/fs/cgroup/cpu.max")
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace();
            let quota = it.next()?;
            if quota == "max" {
                return None;
            }
            let q: f64 = quota.parse().ok()?;
            let period: f64 = it.next().and_then(|p| p.parse().ok()).unwrap_or(100_000.0);
            (period > 0.0).then(|| q / period)
        });
    (mem, cpu)
}

#[cfg(not(target_os = "linux"))]
fn cgroup_limits() -> (Option<u64>, Option<f64>) {
    (None, None)
}

/// Best-effort virtualization/container hint. Reads only class markers (a
/// `/.dockerenv` flag file, cgroup names, the `hypervisor` cpuinfo flag) and
/// never stores their contents. Defaults to `"unknown"`.
#[cfg(target_os = "linux")]
fn virt_hint() -> String {
    if std::path::Path::new("/.dockerenv").exists() {
        return "docker".to_string();
    }
    if let Ok(cg) = std::fs::read_to_string("/proc/1/cgroup") {
        let l = cg.to_ascii_lowercase();
        if l.contains("docker") {
            return "docker".to_string();
        }
        if l.contains("kubepods") || l.contains("containerd") || l.contains("lxc") {
            return "container".to_string();
        }
    }
    if let Ok(ci) = std::fs::read_to_string("/proc/cpuinfo") {
        if ci.contains("hypervisor") {
            return "vm".to_string();
        }
    }
    "unknown".to_string()
}

#[cfg(not(target_os = "linux"))]
fn virt_hint() -> String {
    "unknown".to_string()
}

/// NUMA node count from `/sys/devices/system/node/node<N>` (Linux); `1`
/// elsewhere or when undetectable.
#[cfg(target_os = "linux")]
fn numa_nodes() -> u32 {
    let n = std::fs::read_dir("/sys/devices/system/node")
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|name| {
                            name.len() > 4
                                && name.starts_with("node")
                                && name[4..].bytes().all(|b| b.is_ascii_digit())
                        })
                        .unwrap_or(false)
                })
                .count() as u32
        })
        .unwrap_or(0);
    n.max(1)
}

#[cfg(not(target_os = "linux"))]
fn numa_nodes() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};
    use p2p_proto::NodeId;
    use p2p_trust::verify_system_profile;
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

    #[test]
    fn collects_a_plausible_profile_bound_to_signer() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let budget = BudgetConfig {
            memory_bytes: 8 * 1024 * 1024 * 1024,
            threads: 6,
            ..BudgetConfig::default()
        };
        let p = collect_system_profile(&signer, &budget, "mock-1", "0.3.0");

        // Bound to the collecting identity.
        assert_eq!(p.node_id, signer.node_id());
        // Donated budget is carried through verbatim (distinct from physical RAM).
        assert_eq!(p.donated_budget_mem_bytes, 8 * 1024 * 1024 * 1024);
        assert_eq!(p.donated_budget_threads, 6);
        // Build versions threaded through.
        assert_eq!(p.engine_version, "mock-1");
        assert_eq!(p.extension_version, "0.3.0");
        // Real machine-class fields are populated on any supported host.
        assert!(!p.cpu_arch.is_empty(), "cpu_arch should be detected");
        assert!(p.ram_total_bytes > 0, "ram_total should be detected");
        assert!(p.cpu_logical_cores >= 1);
        // Once signed, it verifies.
        let signed = p2p_trust::sign_system_profile(p, &signer);
        assert!(verify_system_profile(&signed));
    }

    /// The collected profile must serialize without any PII markers (the same
    /// guard as the proto-level test, but over a REAL collected snapshot).
    #[test]
    fn collected_profile_has_no_pii() {
        let signer = TestSigner(SigningKey::generate(&mut OsRng));
        let p = collect_system_profile(&signer, &BudgetConfig::default(), "mock-1", "0.3.0");
        let json = serde_json::to_string(&p).unwrap();
        for forbidden in [
            "hostname", "fqdn", "username", "ip_addr", "mac_addr", "machine_id",
            "serial", "home/", "/Users/", "geolocation",
        ] {
            assert!(
                !json.to_ascii_lowercase().contains(&forbidden.to_ascii_lowercase()),
                "collected SystemProfile must not contain `{forbidden}`"
            );
        }
    }
}
