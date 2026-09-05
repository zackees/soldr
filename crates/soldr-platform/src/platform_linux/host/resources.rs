//! Linux host resources: CPU topology (sysfs/cpuinfo). The Win32
//! process/commit probes have no Linux analogue and answer `None`.

use std::sync::OnceLock;

/// Resolve the cgroup-v2 directory that owns this process.
///
/// Containers may place a process below the mount root. Reading counters from
/// `/sys/fs/cgroup` directly in that case observes the parent rather than the
/// budget and OOM events that actually govern Soldr.
pub fn cgroup_v2_dir() -> Option<std::path::PathBuf> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup_v2_dir_from(&membership, std::path::Path::new("/sys/fs/cgroup"))
}

fn cgroup_v2_dir_from(
    membership: &str,
    mount: &std::path::Path,
) -> Option<std::path::PathBuf> {
    membership.lines().find_map(|line| {
        let relative = line.strip_prefix("0::")?.trim();
        Some(mount.join(relative.trim_start_matches('/')))
    })
}

/// Physical CPU cores on this machine, or `None` when the topology could
/// not be read. Memoized: the daemon asks once at startup.
pub fn physical_cores() -> Option<usize> {
    static CACHED: OnceLock<Option<usize>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        cores_from_sysfs(std::path::Path::new("/sys/devices/system/cpu"))
            .or_else(|| cores_from_cpuinfo(&std::fs::read_to_string("/proc/cpuinfo").ok()?))
            .filter(|cores| *cores > 0)
    })
}

/// Every hardware thread publishes the sibling set it belongs to, and
/// siblings of one physical core publish the *same* list. So the number
/// of distinct lists is the number of physical cores — no parsing of
/// the list contents required.
fn cores_from_sysfs(cpu_root: &std::path::Path) -> Option<usize> {
    use std::collections::HashSet;

    let mut sibling_sets: HashSet<String> = HashSet::new();
    for entry in std::fs::read_dir(cpu_root).ok()? {
        let path = entry.ok()?.path();
        let is_cpu_dir = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit()));
        if !is_cpu_dir {
            continue;
        }
        let siblings = std::fs::read_to_string(path.join("topology/thread_siblings_list")).ok()?;
        sibling_sets.insert(siblings.trim().to_owned());
    }
    (!sibling_sets.is_empty()).then_some(sibling_sets.len())
}

/// Fall back to `/proc/cpuinfo`: count distinct `(physical id, core id)`
/// pairs.
fn cores_from_cpuinfo(contents: &str) -> Option<usize> {
    use std::collections::HashSet;

    let mut cores = HashSet::new();
    let mut package: Option<String> = None;
    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        // A `(physical id, core id)` pair is complete only when the
        // core-id line arrives. Recording on the physical-id line would
        // pair the new package with a stale core id from the previous
        // processor section (and vice versa).
        match key.trim() {
            "physical id" => package = Some(value),
            "core id" => {
                if let Some(package) = package.as_ref() {
                    cores.insert((package.clone(), value));
                }
            }
            _ => continue,
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

/// The ToolHelp process walk is Windows-only; Linux callers get `None`
/// and keep only their neutral fields.
pub fn process_table() -> Option<Vec<(u32, String)>> {
    None
}

/// `GlobalMemoryStatusEx` is Windows-only; Linux callers get `None`.
pub fn commit_charge_mb() -> Option<(u64, u64)> {
    None
}

/// Resident set size for `pid`, in bytes, read from `/proc/<pid>/status`.
///
/// `VmRSS` (not `/proc/<pid>/statm`'s resident page count) is used because
/// it is already in kB and self-documenting in the source file, at the
/// trivial cost of one more `str::parse`. `None` if the process has exited
/// or `/proc` is unreadable (e.g. a sandboxed host with no procfs mount) —
/// never confuse "unreadable" with a live "0 bytes" answer.
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_vm_rss_kb(&status)?.checked_mul(1024)
}

/// Pure parser split out of [`process_rss_bytes`] so the `VmRSS:` line
/// format can be unit-tested without a real `/proc/<pid>/status` file.
fn parse_vm_rss_kb(status: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?;
        value.split_whitespace().next()?.parse::<u64>().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn nested_cgroup_v2_membership_resolves_below_the_mount() {
        let mount = std::path::Path::new("/sys/fs/cgroup");
        assert_eq!(
            cgroup_v2_dir_from("0::/actions_job/step\n", mount),
            Some(mount.join("actions_job/step"))
        );
    }

    #[test]
    fn vm_rss_kb_parses_the_status_line_and_ignores_neighbors() {
        let status = "Name:\tsoldr-daemon\nVmSize:\t  999999 kB\nVmRSS:\t   524288 kB\nThreads:\t37\n";
        assert_eq!(parse_vm_rss_kb(status), Some(524288));
    }

    #[test]
    fn process_rss_bytes_reads_this_process_own_status() {
        // The one thing this can assert without a fixture: the current
        // process is definitely alive and definitely holds > 0 bytes.
        let rss = process_rss_bytes(std::process::id()).expect("read own /proc/<pid>/status");
        assert!(rss > 0, "own RSS must be nonzero, got {rss}");
    }

    #[test]
    fn cpuinfo_parser_counts_distinct_packages_and_cores() {
        let cpuinfo = "\
processor       : 0
physical id     : 0
core id         : 0

processor       : 1
physical id     : 0
core id         : 0

processor       : 2
physical id     : 0
core id         : 1

processor       : 3
physical id     : 1
core id         : 0
";
        assert_eq!(cores_from_cpuinfo(cpuinfo), Some(3));
    }

    #[test]
    fn sysfs_parser_counts_distinct_sibling_lists() {
        let temp = std::env::temp_dir().join(format!("soldr-platform-cpu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        for (cpu, siblings) in [("cpu0", "0,2"), ("cpu1", "1,3"), ("cpu2", "0,2"), ("cpu3", "1,3")] {
            let topo = temp.join(cpu).join("topology");
            std::fs::create_dir_all(&topo).unwrap();
            let mut file = std::fs::File::create(topo.join("thread_siblings_list")).unwrap();
            writeln!(file, "{siblings}").unwrap();
        }
        std::fs::write(temp.join("cpufreq"), b"").unwrap(); // non-cpu dir: ignored
        assert_eq!(cores_from_sysfs(&temp), Some(2));
        std::fs::remove_dir_all(&temp).ok();
    }
}
