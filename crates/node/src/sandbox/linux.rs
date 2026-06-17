//! Linux (and Android) sandbox policy builders for the OS sandbox
//! (architecture §9.4): cgroups v2 (memory/CPU/pids caps), seccomp-bpf (syscall
//! filtering) and an nftables network-egress allow-list.
//!
//! These builders are **pure functions** of the resolved [`SandboxSpec`], so
//! they are unit-tested on any host (including this macOS machine). **Actually
//! installing** a cgroup, a seccomp filter, or an nftables ruleset requires a
//! Linux/Android host (and usually elevated privilege / cgroup delegation); the
//! enforcement entry point [`attach_seccomp_pre_exec`] is therefore
//! `#[cfg]`-gated to Linux/Android and is a thin, documented hook a Linux
//! executor wires up.

use super::{EgressAllowList, SandboxSpec};

// ---------------------------------------------------------------------------
// cgroups v2
// ---------------------------------------------------------------------------

/// A cgroup v2 resource policy: the interface-file values a controller would
/// write under the job's cgroup directory (e.g. `/sys/fs/cgroup/<job>/`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CgroupV2Policy {
    /// `memory.max` (hard memory ceiling), bytes. `None` ⇒ `"max"` (no cap).
    pub memory_max: Option<u64>,
    /// `memory.swap.max`, bytes. `None` ⇒ leave default.
    pub memory_swap_max: Option<u64>,
    /// `cpu.max` = `(quota_us, period_us)`. `None` ⇒ `"max <period>"`.
    pub cpu_max: Option<(u64, u64)>,
    /// `pids.max` (fork-bomb guard). `None` ⇒ `"max"`.
    pub pids_max: Option<u64>,
}

impl CgroupV2Policy {
    /// Derive a cgroup policy from the resolved spec: RAM → `memory.max`,
    /// process cap → `pids.max`. (CPU rate is set separately via
    /// [`CgroupV2Policy::with_cpu_quota`] since the donated budget is expressed
    /// as threads, not a CPU percentage.) Swap is pinned to `0` when a memory
    /// cap is present so a hostile query cannot evade the RAM ceiling via swap.
    pub fn from_spec(spec: &SandboxSpec) -> Self {
        let memory_max = spec.limits.memory_bytes;
        Self {
            memory_max,
            memory_swap_max: memory_max.map(|_| 0),
            cpu_max: None,
            pids_max: spec.limits.max_processes,
        }
    }

    /// Set a CPU rate cap from a thread count: `threads` cores over a 100 ms
    /// period (`quota = threads * period`).
    pub fn with_cpu_quota(mut self, threads: u32) -> Self {
        if threads > 0 {
            let period_us = 100_000u64;
            self.cpu_max = Some((threads as u64 * period_us, period_us));
        }
        self
    }

    /// Render the `(interface_file, value)` pairs a controller writes. Order is
    /// stable so the output is deterministic/testable.
    pub fn interface_files(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        out.push((
            "memory.max",
            self.memory_max
                .map(|v| v.to_string())
                .unwrap_or_else(|| "max".into()),
        ));
        if let Some(s) = self.memory_swap_max {
            out.push(("memory.swap.max", s.to_string()));
        }
        out.push((
            "cpu.max",
            match self.cpu_max {
                Some((q, p)) => format!("{q} {p}"),
                None => "max 100000".into(),
            },
        ));
        out.push((
            "pids.max",
            self.pids_max
                .map(|v| v.to_string())
                .unwrap_or_else(|| "max".into()),
        ));
        out
    }
}

// ---------------------------------------------------------------------------
// seccomp-bpf
// ---------------------------------------------------------------------------

/// Default action for syscalls not on the allow-list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompAction {
    /// Return `EPERM` (fail the syscall) — safer for compatibility.
    Errno,
    /// Kill the offending process — strictest.
    KillProcess,
}

/// A seccomp-bpf policy: a default action plus the allow-listed syscalls a
/// locked-down DuckDB compute job legitimately needs. Network syscalls are only
/// included when egress is permitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeccompPolicy {
    pub default_action: SeccompAction,
    pub allowed_syscalls: Vec<&'static str>,
}

/// Syscalls a CPU/RAM-bound analytical query needs regardless of network.
const BASELINE_SYSCALLS: &[&str] = &[
    // memory
    "mmap",
    "munmap",
    "mremap",
    "mprotect",
    "brk",
    "madvise",
    // file io (reads of allowed paths + scratch writes)
    "read",
    "write",
    "pread64",
    "pwrite64",
    "readv",
    "writev",
    "lseek",
    "openat",
    "open",
    "close",
    "fstat",
    "stat",
    "lstat",
    "newfstatat",
    "fsync",
    "fdatasync",
    "ftruncate",
    "unlink",
    "unlinkat",
    "getdents64",
    // threads / futexes (DuckDB is multi-threaded)
    "clone",
    "clone3",
    "futex",
    "set_robust_list",
    "get_robust_list",
    "rseq",
    "sched_yield",
    "sched_getaffinity",
    "gettid",
    // time / misc
    "clock_gettime",
    "clock_nanosleep",
    "nanosleep",
    "getrandom",
    "getpid",
    "exit",
    "exit_group",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "prlimit64",
    "uname",
    "sysinfo",
    "epoll_create1",
    "epoll_ctl",
    "epoll_wait",
    "eventfd2",
    "pipe2",
];

/// Network syscalls additionally allowed when egress is permitted (the *host*
/// scoping is enforced by nftables + scoped credentials, not seccomp).
const NETWORK_SYSCALLS: &[&str] = &[
    "socket",
    "connect",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "setsockopt",
    "getsockopt",
    "getsockname",
    "getpeername",
    "shutdown",
    "bind",
    "poll",
    "ppoll",
    "select",
];

impl SeccompPolicy {
    /// Compute the baseline allow-list for a compute job. When `allow_network`
    /// is false, NO socket/connect syscalls are permitted — a hostile query
    /// physically cannot open a network connection (the missing control for
    /// remote reads, complementing DuckDB's all-or-nothing flag).
    pub fn compute_baseline(allow_network: bool) -> Self {
        let mut allowed: Vec<&'static str> = BASELINE_SYSCALLS.to_vec();
        if allow_network {
            allowed.extend_from_slice(NETWORK_SYSCALLS);
        }
        Self {
            default_action: SeccompAction::Errno,
            allowed_syscalls: allowed,
        }
    }

    pub fn allows(&self, syscall: &str) -> bool {
        self.allowed_syscalls.contains(&syscall)
    }
}

// ---------------------------------------------------------------------------
// nftables egress allow-list
// ---------------------------------------------------------------------------

/// Generate an `nft`-syntax ruleset that drops all outbound traffic except to
/// the allow-listed destinations (TCP to the configured storage endpoints, plus
/// DNS). When `allow_network` is false, ALL egress is dropped. Pure + testable;
/// a Linux executor pipes this into `nft -f -` for the job's network namespace.
///
/// Hard SSRF / metadata defense applied FIRST, unconditionally (even before the
/// allow-list, and even when `allow_network` is true): the job can NEVER reach
/// the cloud instance-metadata endpoint / link-local range (`169.254.0.0/16`,
/// incl. `169.254.169.254`), IPv6 link-local (`fe80::/10`), or loopback
/// (`127.0.0.0/8`, `::1`) — so it cannot exfiltrate IAM/instance credentials or
/// pivot to host-local admin services. NOTE: because loopback egress is dropped,
/// a Linux executor must give the job a non-loopback DNS resolver (e.g. inside
/// the job's network namespace) rather than relying on `127.0.0.53`.
///
/// Host-name based rules cannot be expressed in nftables directly (it matches
/// IPs/ports); we emit per-port `tcp dport` accepts and rely on the scoped,
/// short-lived credentials + DNS pinning for host scoping. Resolved IPs can be
/// added by the executor when known.
pub fn nftables_egress_rules(egress: &EgressAllowList, allow_network: bool) -> Vec<String> {
    let mut rules = vec![
        "table inet p2p_sandbox {".to_string(),
        "  chain output {".to_string(),
        "    type filter hook output priority 0; policy drop;".to_string(),
        // SSRF / metadata / loopback defense, FIRST so it wins over any accept.
        "    ip daddr 169.254.0.0/16 drop".to_string(),
        "    ip daddr 127.0.0.0/8 drop".to_string(),
        "    ip6 daddr fe80::/10 drop".to_string(),
        "    ip6 daddr ::1 drop".to_string(),
        // Allow return traffic of already-permitted connections.
        "    ct state established,related accept".to_string(),
    ];
    if allow_network && !egress.is_empty() {
        // DNS so the allowed hostnames resolve.
        rules.push("    udp dport 53 accept".to_string());
        rules.push("    tcp dport 53 accept".to_string());
        let mut ports: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
        for ep in &egress.endpoints {
            if let Some(p) = ep.port {
                ports.insert(p);
            }
        }
        if ports.is_empty() {
            rules.push("    tcp dport 443 accept".to_string());
        } else {
            for p in ports {
                rules.push(format!("    tcp dport {p} accept"));
            }
        }
    }
    rules.push("  }".to_string());
    rules.push("}".to_string());
    rules
}

// ---------------------------------------------------------------------------
// Enforcement hook (Linux/Android only)
// ---------------------------------------------------------------------------

/// Best-effort Linux/Android hardening attached to a child job command: set
/// `no_new_privs` (so a seccomp filter can be installed unprivileged and cannot
/// be bypassed via setuid binaries). The full seccomp-bpf program build from
/// [`SeccompPolicy`] and cgroup placement from [`CgroupV2Policy`] are wired by
/// the Linux executor (they require either `libseccomp` or a hand-assembled BPF
/// program and a delegated cgroup); this hook installs the safe, dependency-free
/// prerequisite.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn attach_seccomp_pre_exec(cmd: &mut std::process::Command, _spec: &SandboxSpec) {
    use std::os::unix::process::CommandExt;
    // SAFETY: only an async-signal-safe `prctl` syscall.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{EgressAllowList, EgressEndpoint, ResourceLimits, SandboxSpec};

    fn spec(
        mem: Option<u64>,
        procs: Option<u64>,
        allow_network: bool,
        ports: &[u16],
    ) -> SandboxSpec {
        SandboxSpec {
            limits: ResourceLimits {
                memory_bytes: mem,
                max_processes: procs,
                ..ResourceLimits::default()
            },
            egress: EgressAllowList {
                endpoints: ports
                    .iter()
                    .map(|p| EgressEndpoint::new("s3.amazonaws.com", Some(*p)))
                    .collect(),
            },
            read_only_paths: vec![],
            writable_paths: vec!["/tmp/job".into()],
            allow_network,
        }
    }

    #[test]
    fn cgroup_policy_renders_interface_files() {
        let p = CgroupV2Policy::from_spec(&spec(Some(1_073_741_824), Some(10), false, &[]))
            .with_cpu_quota(2);
        let files = p.interface_files();
        let map: std::collections::HashMap<_, _> = files.into_iter().collect();
        assert_eq!(map["memory.max"], "1073741824");
        assert_eq!(map["memory.swap.max"], "0"); // swap pinned off under a RAM cap
        assert_eq!(map["pids.max"], "10");
        assert_eq!(map["cpu.max"], "200000 100000"); // 2 cores * 100ms
    }

    #[test]
    fn cgroup_policy_unbounded_when_no_limits() {
        let p = CgroupV2Policy::from_spec(&spec(None, None, false, &[]));
        let map: std::collections::HashMap<_, _> = p.interface_files().into_iter().collect();
        assert_eq!(map["memory.max"], "max");
        assert_eq!(map["pids.max"], "max");
        assert!(!map.contains_key("memory.swap.max"));
    }

    #[test]
    fn seccomp_omits_network_when_egress_denied() {
        let p = SeccompPolicy::compute_baseline(false);
        assert!(p.allows("read"));
        assert!(p.allows("mmap"));
        assert!(p.allows("futex"));
        // No way to open a socket when network egress is denied.
        assert!(!p.allows("socket"));
        assert!(!p.allows("connect"));
        assert_eq!(p.default_action, SeccompAction::Errno);
    }

    #[test]
    fn seccomp_includes_network_when_egress_allowed() {
        let p = SeccompPolicy::compute_baseline(true);
        assert!(p.allows("socket"));
        assert!(p.allows("connect"));
        assert!(p.allows("read"));
    }

    #[test]
    fn nftables_drops_all_when_network_denied() {
        let rules = nftables_egress_rules(&EgressAllowList::default(), false);
        let joined = rules.join("\n");
        assert!(joined.contains("policy drop"));
        assert!(!joined.contains("tcp dport 443"));
        assert!(!joined.contains("dport 53"));
        // The SSRF/metadata/loopback drops are present even with network denied.
        assert!(joined.contains("ip daddr 169.254.0.0/16 drop"));
        assert!(joined.contains("ip daddr 127.0.0.0/8 drop"));
    }

    #[test]
    fn nftables_allows_only_configured_ports() {
        let s = spec(None, None, true, &[443, 9000]);
        let rules = nftables_egress_rules(&s.egress, s.allow_network);
        let joined = rules.join("\n");
        assert!(joined.contains("policy drop"));
        assert!(joined.contains("tcp dport 443 accept"));
        assert!(joined.contains("tcp dport 9000 accept"));
        assert!(joined.contains("udp dport 53 accept")); // DNS
        assert!(!joined.contains("tcp dport 21"));
    }

    #[test]
    fn nftables_blocks_metadata_and_loopback_even_when_network_allowed() {
        // Even with egress permitted to storage, the cloud metadata endpoint,
        // link-local range, and loopback must be unreachable (SSRF / IMDS
        // credential-theft defense). The drops must precede the dport accepts.
        let s = spec(None, None, true, &[443]);
        let rules = nftables_egress_rules(&s.egress, s.allow_network);
        let joined = rules.join("\n");
        assert!(joined.contains("ip daddr 169.254.0.0/16 drop"));
        assert!(joined.contains("ip daddr 127.0.0.0/8 drop"));
        assert!(joined.contains("ip6 daddr fe80::/10 drop"));
        assert!(joined.contains("ip6 daddr ::1 drop"));
        let meta_drop = rules
            .iter()
            .position(|r| r.contains("169.254.0.0/16 drop"))
            .unwrap();
        let port_accept = rules
            .iter()
            .position(|r| r.contains("tcp dport 443 accept"))
            .unwrap();
        assert!(
            meta_drop < port_accept,
            "metadata drop must precede the port accepts so it cannot be bypassed"
        );
    }
}
