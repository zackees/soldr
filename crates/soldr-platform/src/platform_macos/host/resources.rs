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

/// Resident set size for `pid`, in bytes.
///
/// Shells out to `ps` rather than the `mach_task_self`/`task_info` FFI
/// pair: this file's `detect_cores` already establishes that a one-shot
/// subprocess probe is an accepted pattern here, and `ps -o rss=` needs no
/// new FFI surface or `libc` struct layout to keep in sync with the SDK.
/// `ps` reports `rss` in kB. `None` if the process has exited or the
/// subprocess could not be run.
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}
