//! Windows sandbox policy builders for the OS sandbox (architecture §9.4):
//! **Job Objects** (memory / CPU / active-process caps with kill-on-close),
//! **Restricted Tokens / AppContainer** for filesystem + privilege isolation,
//! and **Windows Filtering Platform / firewall** egress rules.
//!
//! Windows has no POSIX `rlimit`, so resource caps are expressed as a
//! [`JobObjectLimits`] applied via a Job Object. The builders here are **pure
//! functions** of the resolved [`SandboxSpec`] and are unit-tested on any host
//! (including this macOS machine). **Actually creating** a Job Object /
//! restricted token / firewall rule requires a Windows host; that enforcement
//! lives behind `#[cfg(windows)]` in [`create_configured_job_object`] /
//! [`assign_process_to_job`] (compiled only on Windows; verified by inspection
//! here — see the task report's "implemented-for-target" caveat).
//!
//! ## Honest caveat — egress on Windows
//! Per-**process** network-egress filtering is harder on Windows than the Unix
//! network-namespace approach: WFP filters and `netsh advfirewall` rules are
//! keyed by program path / app-id and remote address/port, not by an ephemeral
//! child process. We generate program- and port-scoped outbound rules and
//! document that tight per-job egress typically needs an AppContainer SID-scoped
//! WFP filter installed by a privileged helper.

use super::{EgressAllowList, ResourceLimits, SandboxSpec};

// ---------------------------------------------------------------------------
// Job Object limits
// ---------------------------------------------------------------------------

/// Resource caps expressed as a Windows Job Object limit set. A `None` field
/// installs no cap for that resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JobObjectLimits {
    /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION.ProcessMemoryLimit` (bytes): the
    /// per-process commit cap — the Windows analogue of `RLIMIT_AS`.
    pub process_memory_bytes: Option<u64>,
    /// `JobMemoryLimit` (bytes): the whole-job commit cap.
    pub job_memory_bytes: Option<u64>,
    /// `BasicLimitInformation.ActiveProcessLimit`: max concurrent processes.
    pub active_process_limit: Option<u32>,
    /// `PerJobUserTimeLimit` in 100-nanosecond units (CPU time, the analogue of
    /// `RLIMIT_CPU`).
    pub per_job_user_time_100ns: Option<u64>,
    /// Optional CPU rate cap as a percentage `1..=100`
    /// (`JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`).
    pub cpu_rate_percent: Option<u8>,
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: terminate all job processes when
    /// the job handle closes — guarantees no orphaned runaway query survives.
    pub kill_on_job_close: bool,
}

impl JobObjectLimits {
    /// Derive Job Object caps from the resolved limits. Memory maps to BOTH the
    /// per-process and whole-job commit caps; the process count maps to the
    /// active-process limit; CPU seconds map to the per-job user-time limit.
    /// `kill_on_job_close` is always set so a runaway query is reaped.
    pub fn from_limits(limits: &ResourceLimits) -> Self {
        Self {
            process_memory_bytes: limits.memory_bytes,
            job_memory_bytes: limits.memory_bytes,
            active_process_limit: limits.max_processes.map(|p| p.min(u32::MAX as u64) as u32),
            per_job_user_time_100ns: limits.cpu_seconds.map(|s| s.saturating_mul(10_000_000)),
            cpu_rate_percent: None,
            kill_on_job_close: true,
        }
    }

    pub fn from_spec(spec: &SandboxSpec) -> Self {
        Self::from_limits(&spec.limits)
    }

    /// Add a CPU rate cap (percentage of total CPU, `1..=100`).
    pub fn with_cpu_rate_percent(mut self, pct: u8) -> Self {
        if (1..=100).contains(&pct) {
            self.cpu_rate_percent = Some(pct);
        }
        self
    }
}

// ---------------------------------------------------------------------------
// Filesystem + privilege isolation (Restricted Token / AppContainer)
// ---------------------------------------------------------------------------

/// A description of the filesystem + privilege isolation a restricted token or
/// AppContainer profile should enforce. Pure descriptor consumed by the Windows
/// executor (which builds the SID-scoped ACLs / capability set).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowsIsolationPolicy {
    /// Directories the job may read.
    pub read_only_paths: Vec<String>,
    /// Directories the job may write (scratch/temp).
    pub writable_paths: Vec<String>,
    /// Run under a low-integrity AppContainer (vs. a plain restricted token).
    pub use_app_container: bool,
    /// AppContainer capabilities to grant. Egress requires the
    /// `internetClient` capability; when network is denied we grant none, so the
    /// container cannot reach the network at all.
    pub capabilities: Vec<&'static str>,
}

impl WindowsIsolationPolicy {
    pub fn from_spec(spec: &SandboxSpec) -> Self {
        let capabilities = if spec.allow_network {
            vec!["internetClient"]
        } else {
            // No network capability ⇒ the AppContainer cannot open the network.
            vec![]
        };
        Self {
            read_only_paths: spec.read_only_paths.clone(),
            writable_paths: spec.writable_paths.clone(),
            use_app_container: true,
            capabilities,
        }
    }
}

// ---------------------------------------------------------------------------
// Firewall / WFP egress rules
// ---------------------------------------------------------------------------

/// Generate `netsh advfirewall` rules that scope a job program's outbound
/// traffic. When `allow_network` is false a single block-all-outbound rule is
/// emitted; otherwise per-port allow rules for the configured storage endpoints
/// (plus DNS) precede an explicit block. `program` is the sandboxed executable
/// path the rules are keyed to. Pure + testable.
///
/// NOTE (documented limitation): these rules are program-path scoped, not
/// ephemeral-process scoped, and match remote port (not hostname). Tight
/// per-job egress on Windows realistically needs an AppContainer SID-scoped WFP
/// filter installed by a privileged helper.
pub fn firewall_egress_rules(
    egress: &EgressAllowList,
    allow_network: bool,
    program: &str,
) -> Vec<String> {
    let base = "netsh advfirewall firewall add rule";
    let mut rules = Vec::new();
    if !allow_network || egress.is_empty() {
        rules.push(format!(
            "{base} name=\"p2p-sandbox-deny\" dir=out action=block program=\"{program}\" enable=yes"
        ));
        return rules;
    }
    // DNS resolution.
    rules.push(format!(
        "{base} name=\"p2p-sandbox-dns\" dir=out action=allow protocol=UDP remoteport=53 program=\"{program}\" enable=yes"
    ));
    let mut ports: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    for ep in &egress.endpoints {
        if let Some(p) = ep.port {
            ports.insert(p);
        }
    }
    if ports.is_empty() {
        ports.insert(443);
    }
    for p in ports {
        rules.push(format!(
            "{base} name=\"p2p-sandbox-allow-{p}\" dir=out action=allow protocol=TCP remoteport={p} program=\"{program}\" enable=yes"
        ));
    }
    // Explicit catch-all block after the allows.
    rules.push(format!(
        "{base} name=\"p2p-sandbox-deny\" dir=out action=block program=\"{program}\" enable=yes"
    ));
    rules
}

// ---------------------------------------------------------------------------
// Enforcement (Windows only) — Job Objects via windows-sys
// ---------------------------------------------------------------------------

/// Create a Job Object configured with `limits` and return its handle. The
/// executor spawns the job process **suspended**, calls
/// [`assign_process_to_job`], then resumes it; closing the returned handle with
/// `kill_on_job_close` reaps any survivors.
///
/// Implemented for the Windows target and compiled only there; verified by
/// inspection on this macOS host (no Windows std available — see report).
#[cfg(windows)]
pub fn create_configured_job_object(
    limits: &JobObjectLimits,
) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };

    // SAFETY: standard Win32 Job Object setup; all pointers point at locals that
    // outlive the calls.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        let mut flags: u32 = 0;
        if limits.kill_on_job_close {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if let Some(p) = limits.active_process_limit {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = p;
        }
        if let Some(t) = limits.per_job_user_time_100ns {
            flags |= JOB_OBJECT_LIMIT_JOB_TIME;
            info.BasicLimitInformation.PerJobUserTimeLimit = t as i64;
        }
        if let Some(m) = limits.process_memory_bytes {
            flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = m as usize;
        }
        if let Some(m) = limits.job_memory_bytes {
            flags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = m as usize;
        }
        info.BasicLimitInformation.LimitFlags = flags;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            windows_sys::Win32::Foundation::CloseHandle(job);
            return Err(err);
        }
        Ok(job)
    }
}

/// Assign a spawned (ideally suspended) process to a Job Object.
#[cfg(windows)]
pub fn assign_process_to_job(
    job: windows_sys::Win32::Foundation::HANDLE,
    process: windows_sys::Win32::Foundation::HANDLE,
) -> std::io::Result<()> {
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    // SAFETY: both handles are owned by the caller for the duration of the call.
    unsafe {
        if AssignProcessToJobObject(job, process) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{EgressAllowList, EgressEndpoint, ResourceLimits, SandboxSpec};

    fn limits() -> ResourceLimits {
        ResourceLimits {
            memory_bytes: Some(1_073_741_824),
            cpu_seconds: Some(30),
            max_processes: Some(12),
            ..ResourceLimits::default()
        }
    }

    #[test]
    fn job_object_limits_from_resolved_limits() {
        let j = JobObjectLimits::from_limits(&limits());
        assert_eq!(j.process_memory_bytes, Some(1_073_741_824));
        assert_eq!(j.job_memory_bytes, Some(1_073_741_824));
        assert_eq!(j.active_process_limit, Some(12));
        assert_eq!(j.per_job_user_time_100ns, Some(300_000_000)); // 30s * 1e7
        assert!(j.kill_on_job_close);
    }

    #[test]
    fn job_object_cpu_rate_clamped() {
        let j = JobObjectLimits::default().with_cpu_rate_percent(150);
        assert_eq!(j.cpu_rate_percent, None);
        let j = JobObjectLimits::default().with_cpu_rate_percent(50);
        assert_eq!(j.cpu_rate_percent, Some(50));
    }

    fn spec(allow_network: bool, ports: &[u16]) -> SandboxSpec {
        SandboxSpec {
            limits: limits(),
            egress: EgressAllowList {
                endpoints: ports
                    .iter()
                    .map(|p| EgressEndpoint::new("s3.amazonaws.com", Some(*p)))
                    .collect(),
            },
            read_only_paths: vec!["C:\\fixtures".into()],
            writable_paths: vec!["C:\\Temp\\job".into()],
            allow_network,
        }
    }

    #[test]
    fn isolation_grants_no_network_capability_when_denied() {
        let p = WindowsIsolationPolicy::from_spec(&spec(false, &[]));
        assert!(p.use_app_container);
        assert!(p.capabilities.is_empty());
        assert_eq!(p.read_only_paths, vec!["C:\\fixtures".to_string()]);
    }

    #[test]
    fn isolation_grants_internet_client_when_allowed() {
        let p = WindowsIsolationPolicy::from_spec(&spec(true, &[443]));
        assert_eq!(p.capabilities, vec!["internetClient"]);
    }

    #[test]
    fn firewall_blocks_all_when_network_denied() {
        let rules = firewall_egress_rules(&EgressAllowList::default(), false, "C:\\job.exe");
        assert_eq!(rules.len(), 1);
        assert!(rules[0].contains("action=block"));
        assert!(rules[0].contains("dir=out"));
    }

    #[test]
    fn firewall_allows_configured_ports_then_blocks() {
        let s = spec(true, &[443, 9000]);
        let rules = firewall_egress_rules(&s.egress, s.allow_network, "C:\\job.exe");
        let joined = rules.join("\n");
        assert!(joined.contains("protocol=UDP remoteport=53")); // DNS
        assert!(joined.contains("protocol=TCP remoteport=443 program=\"C:\\job.exe\""));
        assert!(joined.contains("protocol=TCP remoteport=9000"));
        // last rule is the catch-all block
        assert!(rules.last().unwrap().contains("action=block"));
    }
}
