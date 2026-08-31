//! macOS host resources: CPU topology via sysctl. The Win32
//! process/commit probes have no macOS analogue and answer `None`.

use std::sync::OnceLock;

/// cgroup v2 is a Linux facility.
pub fn cgroup_v2_dir() -> Option<std::path::PathBuf> {
    None
}

/// Physical CPU cores on this machine, or `None` when the topology could
/// not be read. Memoized: the daemon asks once at startup.
pub fn physical_cores() -> Option<usize> {
    static CACHED: OnceLock<Option<usize>> = OnceLock::new();
    *CACHED.get_or_init(|| detect_cores().filter(|cores| *cores > 0))
}

/// `hw.physicalcpu` is the count for *this* process's allowed set,
/// which is what we want; `hw.physicalcpu_max` would ignore a
/// restricted CPU affinity.
fn detect_cores() -> Option<usize> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.physicalcpu"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// The ToolHelp process walk is Windows-only; macOS callers get `None`
/// and keep only their neutral fields.
pub fn process_table() -> Option<Vec<(u32, String)>> {
    None
}

/// `GlobalMemoryStatusEx` is Windows-only; macOS callers get `None`.
pub fn commit_charge_mb() -> Option<(u64, u64)> {
    None
}
