//! Memory observation, process-tree sampling and the kernel-enforced
//! per-test cgroup ceiling (soldr#2885). Linux reads procfs and cgroup v2;
//! macOS reads `vm_stat` and samples the tree with `ps` (the sampled-ceiling
//! fallback, which can overshoot by whatever the tree allocates between
//! samples); other hosts report memory as unavailable.

use soldr_core::core::tool_output::{capture_small_tool_with_sinks, ToolSinks};
use soldr_platform::process::test_child::{self, TestSignal};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

pub fn format_bytes(value: Option<u64>) -> String {
    match value {
        None => "unknown".into(),
        Some(value) if value >= GIB => format!("{:.2} GiB", value as f64 / GIB as f64),
        Some(value) => format!("{:.1} MiB", value as f64 / MIB as f64),
    }
}

pub fn is_linux() -> bool {
    soldr_platform::host::facts::os() == soldr_platform::host::facts::HostOs::Linux
}

pub fn is_macos() -> bool {
    soldr_platform::host::facts::os() == soldr_platform::host::facts::HostOs::MacOs
}

/// How often the sampled ceiling looks at the test's tree: procfs is cheap,
/// a `ps` snapshot of every process is not.
pub fn sample_interval() -> Duration {
    if is_linux() {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(1)
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Stdout of a host probe (`vm_stat`, `ps`). Its stderr is forwarded with the
/// probe's name; the probe is sampled every second, so it is not journaled.
fn probe_stdout(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args);
    let mut stderr = std::io::stderr();
    let sinks = ToolSinks {
        stderr: &mut stderr,
        log_path: None,
    };
    let output = capture_small_tool_with_sinks(&mut command, program, Some(PROBE_TIMEOUT), sinks)
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn read(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn read_int(path: &Path) -> Option<u64> {
    read(path)?.trim().parse().ok()
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

#[derive(Clone, Debug, Default)]
pub struct MemoryObservation {
    pub available_bytes: Option<u64>,
    pub source: &'static str,
    pub cgroup_current_bytes: Option<u64>,
    pub cgroup_limit: Option<String>,
    pub pids_current: Option<u64>,
    pub pids_limit: Option<String>,
}

impl MemoryObservation {
    pub fn describe(&self) -> String {
        let mut text = format!("{} ({})", format_bytes(self.available_bytes), self.source);
        if self.cgroup_current_bytes.is_some() || self.cgroup_limit.is_some() {
            text.push_str(&format!(
                "; cgroup memory.current={} memory.max={}",
                format_bytes(self.cgroup_current_bytes),
                self.cgroup_limit.as_deref().unwrap_or("unknown")
            ));
        }
        text
    }
}

fn own_cgroup_dir() -> Option<PathBuf> {
    let membership = read(Path::new("/proc/self/cgroup")).unwrap_or_default();
    membership.lines().find_map(|line| {
        line.strip_prefix("0::")
            .map(|rest| Path::new("/sys/fs/cgroup").join(rest.trim().trim_start_matches('/')))
    })
}

fn mem_available(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kib| kib * 1024)
}

/// Free + inactive + speculative + purgeable pages from `vm_stat` output.
pub fn vm_stat_available(text: &str) -> Option<u64> {
    let mut lines = text.lines();
    let page_size: u64 = lines
        .next()?
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let mut pages = 0u64;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if matches!(
            key.trim(),
            "Pages free" | "Pages inactive" | "Pages speculative" | "Pages purgeable"
        ) {
            if let Ok(count) = value.trim().trim_end_matches('.').parse::<u64>() {
                pages += count;
            }
        }
    }
    Some(pages * page_size)
}

/// The tighter of host `MemAvailable` and finite cgroup headroom on Linux;
/// `vm_stat`'s reclaimable pages on macOS.
pub fn observe_memory() -> MemoryObservation {
    if !is_linux() {
        let available = is_macos()
            .then(|| probe_stdout("/usr/bin/vm_stat", &[]))
            .flatten()
            .and_then(|text| vm_stat_available(&text));
        return MemoryObservation {
            available_bytes: available,
            source: if available.is_some() {
                "vm_stat"
            } else {
                "unavailable"
            },
            ..MemoryObservation::default()
        };
    }
    let mut available = mem_available(&read(Path::new("/proc/meminfo")).unwrap_or_default());
    let mut source = if available.is_some() {
        "MemAvailable"
    } else {
        "unavailable"
    };
    let mut observation = MemoryObservation::default();
    if let Some(cgroup) = own_cgroup_dir() {
        observation.cgroup_current_bytes = read_int(&cgroup.join("memory.current"));
        observation.cgroup_limit = non_empty(read(&cgroup.join("memory.max")));
        observation.pids_current = read_int(&cgroup.join("pids.current"));
        observation.pids_limit = non_empty(read(&cgroup.join("pids.max")));
        if let (Some(current), Some(limit)) = (
            observation.cgroup_current_bytes,
            observation
                .cgroup_limit
                .as_deref()
                .and_then(|limit| limit.parse::<u64>().ok()),
        ) {
            let headroom = limit.saturating_sub(current);
            if available.is_none_or(|available| headroom < available) {
                available = Some(headroom);
                source = "cgroup headroom";
            }
        }
    }
    observation.available_bytes = available;
    observation.source = source;
    observation
}

#[derive(Clone, Debug, Default)]
pub struct TreeSample {
    pub rss_bytes: u64,
    pub process_count: usize,
    pub pids: Vec<u32>,
}

fn linux_children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for task in tasks.flatten() {
        let listing = read(&task.path().join("children")).unwrap_or_default();
        children.extend(
            listing
                .split_whitespace()
                .filter_map(|child| child.parse::<u32>().ok()),
        );
    }
    children
}

fn linux_rss(pid: u32) -> Option<u64> {
    let statm = read(Path::new(&format!("/proc/{pid}/statm")))?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * test_child::page_size())
}

/// `ps -A -o pid=,ppid=,rss=` -> `{pid: (ppid, rss KiB)}`.
pub fn parse_ps_table(text: &str) -> HashMap<u32, (u32, u64)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [pid, ppid, rss] = fields.as_slice() else {
                return None;
            };
            Some((pid.parse().ok()?, (ppid.parse().ok()?, rss.parse().ok()?)))
        })
        .collect()
}

/// `root` and its descendants in a `ps` table.
pub fn tree_from_table(root: u32, table: &HashMap<u32, (u32, u64)>) -> Option<TreeSample> {
    table.get(&root)?;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, (ppid, _)) in table {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut sample = TreeSample::default();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        sample.rss_bytes += table.get(&pid).map_or(0, |(_, rss)| rss * 1024);
        sample.pids.push(pid);
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    sample.process_count = sample.pids.len();
    Some(sample)
}

/// Resident bytes of `root` and every live descendant: procfs on Linux, a
/// `ps` snapshot elsewhere.
pub fn sample_tree(root: u32) -> Option<TreeSample> {
    if !is_linux() {
        let table = probe_stdout("/bin/ps", &["-A", "-o", "pid=,ppid=,rss="])?;
        return tree_from_table(root, &parse_ps_table(&table));
    }
    let root_rss = linux_rss(root)?;
    let mut sample = TreeSample::default();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let rss = if pid == root {
            Some(root_rss)
        } else {
            linux_rss(pid)
        };
        let Some(rss) = rss else {
            continue;
        };
        sample.rss_bytes += rss;
        sample.pids.push(pid);
        stack.extend(linux_children(pid));
    }
    sample.process_count = sample.pids.len();
    Some(sample)
}

/// SIGKILL `root`'s process group and every descendant still visible.
pub fn kill_tree(root: u32) {
    let sample = sample_tree(root);
    test_child::signal_group(root, TestSignal::Kill);
    for pid in sample.map(|sample| sample.pids).unwrap_or_default() {
        test_child::signal_process(pid, TestSignal::Kill);
    }
}

pub struct CgroupOutcome {
    pub peak_bytes: Option<u64>,
    pub oom_kills: u64,
}

/// Write the ceiling into a fresh per-test leaf. With swap, `memory.max`
/// only bounds the resident part: pin swap to zero, or fail so the caller
/// declines the leaf and the sampled ceiling applies.
pub fn configure_leaf(leaf: &Path, ceiling_bytes: u64) -> std::io::Result<()> {
    std::fs::write(leaf.join("memory.max"), ceiling_bytes.to_string())?;
    let swap_max = leaf.join("memory.swap.max");
    if swap_max.exists() {
        std::fs::write(swap_max, "0")?;
    }
    Ok(())
}

/// A kernel-enforced per-test ceiling below a delegated cgroup v2 root.
pub struct CgroupCeiling {
    pub path: PathBuf,
    pub joined: bool,
}

impl CgroupCeiling {
    pub fn create(root: &Path, ceiling_bytes: u64, owner_pid: u32) -> Option<Self> {
        let controllers = read(&root.join("cgroup.subtree_control")).unwrap_or_default();
        if !controllers.split_whitespace().any(|name| name == "memory") {
            return None;
        }
        for attempt in 0..8 {
            let leaf = root.join(if attempt == 0 {
                format!("snt-{owner_pid}")
            } else {
                format!("snt-{owner_pid}-{attempt}")
            });
            match std::fs::create_dir(&leaf) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
            let configured = configure_leaf(&leaf, ceiling_bytes);
            let ceiling = Self {
                path: leaf,
                joined: false,
            };
            if configured.is_err() {
                ceiling.release();
                return None;
            }
            let _ = std::fs::write(ceiling.path.join("memory.oom.group"), "1");
            return Some(ceiling);
        }
        None
    }

    pub fn procs_path(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    pub fn confirm(&mut self, pid: u32) -> bool {
        let procs = read(&self.procs_path()).unwrap_or_default();
        self.joined = procs
            .split_whitespace()
            .any(|entry| entry == pid.to_string());
        self.joined
    }

    pub fn outcome(&self) -> CgroupOutcome {
        let mut oom_kills = 0;
        for line in read(&self.path.join("memory.events"))
            .unwrap_or_default()
            .lines()
        {
            let (name, value) = line.split_once(' ').unwrap_or((line, ""));
            if matches!(name, "oom_kill" | "oom_group_kill") {
                oom_kills += value.trim().parse::<u64>().unwrap_or(0);
            }
        }
        CgroupOutcome {
            peak_bytes: read_int(&self.path.join("memory.peak")),
            oom_kills,
        }
    }

    pub fn release(&self) {
        if std::fs::remove_dir(&self.path).is_ok() {
            return;
        }
        // Detached descendants still live here: park them in one unlimited
        // sibling so the per-test leaf can go.
        let Some(parent) = self.path.parent() else {
            return;
        };
        let orphans = parent.join("snt-orphans");
        if std::fs::create_dir(&orphans).is_err() && !orphans.is_dir() {
            return;
        }
        for pid in read(&self.procs_path())
            .unwrap_or_default()
            .split_whitespace()
        {
            if std::fs::write(orphans.join("cgroup.procs"), pid).is_err() {
                return;
            }
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
