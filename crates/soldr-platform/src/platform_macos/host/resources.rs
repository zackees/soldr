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

/// Memory the kernel could hand to a new process without paging, in bytes:
/// free + inactive + speculative + purgeable pages from `vm_stat`
/// (soldr#2885). macOS has no `/proc/meminfo`, so this is its counterpart of
/// Linux's `MemAvailable`. Like `detect_cores` it is a one-shot subprocess
/// probe rather than new Mach FFI. `None` when `vm_stat` cannot be run or its
/// output is not understood -- never a guessed zero.
pub fn available_physical_memory_bytes() -> Option<u64> {
    let output = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_vm_stat_available(&String::from_utf8(output.stdout).ok()?)
}

fn parse_vm_stat_available(text: &str) -> Option<u64> {
    let mut lines = text.lines();
    // "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size: u64 = lines
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let mut pages: Option<u64> = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(
            key.trim(),
            "Pages free" | "Pages inactive" | "Pages speculative" | "Pages purgeable"
        ) {
            continue;
        }
        let Ok(count) = value.trim().trim_end_matches('.').parse::<u64>() else {
            continue;
        };
        pages = Some(pages.unwrap_or(0).saturating_add(count));
    }
    pages?.checked_mul(page_size)
}

#[cfg(test)]
mod available_memory_tests {
    use super::*;

    #[test]
    fn vm_stat_available_sums_reclaimable_page_classes() {
        let text = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                               10.\n\
Pages active:                            999.\n\
Pages inactive:                           20.\n\
Pages speculative:                         3.\n\
Pages wired down:                        777.\n\
Pages purgeable:                           2.\n";
        assert_eq!(parse_vm_stat_available(text), Some(35 * 16384));
        assert_eq!(parse_vm_stat_available("garbage"), None);
    }

    #[test]
    fn live_vm_stat_reports_nonzero_available_memory() {
        let available = available_physical_memory_bytes().expect("vm_stat probe");
        assert!(available > 0);
    }
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
