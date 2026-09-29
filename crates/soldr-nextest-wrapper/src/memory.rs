//! Memory observation, process-tree sampling and the kernel-enforced
//! per-test cgroup ceiling. Mirrors `.github/scripts/nextest_memory_guard.py`
//! on Linux; other hosts report memory as unavailable and sample nothing.

use soldr_platform::process::test_child::{self, TestSignal};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// The tighter of host `MemAvailable` and finite cgroup headroom.
pub fn observe_memory() -> MemoryObservation {
    if !is_linux() {
        return MemoryObservation {
            source: "unavailable",
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

/// Resident bytes of `root` and every live descendant (Linux only).
pub fn sample_tree(root: u32) -> Option<TreeSample> {
    if !is_linux() {
        return None;
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
            let configured = std::fs::write(leaf.join("memory.max"), ceiling_bytes.to_string())
                .and_then(|()| {
                    // With swap, `memory.max` only bounds the resident part:
                    // pin swap to zero, or decline so the sampled ceiling applies.
                    let swap_max = leaf.join("memory.swap.max");
                    if swap_max.exists() {
                        std::fs::write(swap_max, "0")
                    } else {
                        Ok(())
                    }
                });
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
