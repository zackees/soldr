//! Best-effort local process-tree termination.
//!
//! This API does not require the running-process daemon. It snapshots the
//! tree, terminates the root first so it cannot create more descendants,
//! then terminates the captured descendants deepest-first. Process start
//! times are retained while waiting so a recycled PID is not treated as
//! the original target. On Windows these are exact kernel creation times;
//! other platforms use the best timestamp exposed by `sysinfo`.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, Instant};

use sysinfo::{Pid, Process, System};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessInstance {
    pid: Pid,
    start_time: u64,
}

impl ProcessInstance {
    fn from_process(pid: Pid, process: &Process) -> io::Result<Self> {
        Ok(Self {
            pid,
            start_time: process_start_key(pid, process)?,
        })
    }

    fn still_matches(self, system: &System) -> bool {
        system.process(self.pid).is_some_and(|process| {
            process_start_key(self.pid, process)
                .is_ok_and(|start_time| start_time == self.start_time)
        })
    }
}

/// Kill the process tree rooted at `pid`.
///
/// Returns the number of distinct process instances for which the OS accepted
/// a kill request. A missing PID is a successful no-op. `timeout` bounds the
/// post-signal wait and retry loop; the initial termination attempt always
/// occurs even when the timeout is zero.
///
/// The tree is a point-in-time snapshot. A process created in the narrow race
/// between enumeration and root termination can escape unless the caller also
/// owns an OS containment primitive such as a Windows Job Object.
pub fn kill_tree(pid: u32, timeout: Duration) -> io::Result<u32> {
    let mut system = System::new();
    system.refresh_processes();

    let root_pid = Pid::from_u32(pid);
    let Some(root_process) = system.process(root_pid) else {
        return Ok(0);
    };
    let root = match ProcessInstance::from_process(root_pid, root_process) {
        Ok(root) => root,
        Err(error) => {
            // The process may have exited between the sysinfo snapshot and
            // the exact identity query. Treat that race like a missing PID,
            // but preserve access-denied and other errors for a live target.
            system.refresh_processes();
            if system.process(root_pid).is_none() {
                return Ok(0);
            }
            return Err(error);
        }
    };

    let mut descendants = Vec::new();
    let mut visited = HashSet::new();
    collect_descendants(&system, root, 1, &mut visited, &mut descendants);
    descendants.sort_unstable_by_key(|(_, depth)| std::cmp::Reverse(*depth));

    // Root first: once it exits it can no longer respawn a descendant that
    // was present in the snapshot. Descendants then go deepest-first.
    let mut targets = Vec::with_capacity(descendants.len() + 1);
    targets.push(root);
    targets.extend(descendants.into_iter().map(|(instance, _)| instance));

    let mut signaled = HashSet::new();
    signal_matching(&system, &targets, &mut signaled);

    let started = Instant::now();
    loop {
        system.refresh_processes();
        let remaining: Vec<_> = targets
            .iter()
            .copied()
            .filter(|target| target.still_matches(&system))
            .collect();
        if remaining.is_empty() || started.elapsed() >= timeout {
            break;
        }

        signal_matching(&system, &remaining, &mut signaled);
        let sleep_for = timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_millis(25));
        if sleep_for.is_zero() {
            break;
        }
        std::thread::sleep(sleep_for);
    }

    Ok(signaled.len() as u32)
}

fn signal_matching(
    system: &System,
    targets: &[ProcessInstance],
    signaled: &mut HashSet<ProcessInstance>,
) {
    for target in targets {
        let Some(process) = system.process(target.pid) else {
            continue;
        };
        if process_start_key(target.pid, process)
            .is_ok_and(|start_time| start_time == target.start_time)
            && process.kill()
        {
            signaled.insert(*target);
        }
    }
}

fn collect_descendants(
    system: &System,
    parent: ProcessInstance,
    depth: usize,
    visited: &mut HashSet<Pid>,
    descendants: &mut Vec<(ProcessInstance, usize)>,
) {
    for (pid, process) in system.processes() {
        if process.parent() != Some(parent.pid) || visited.contains(pid) {
            continue;
        }

        // A process cannot be the child of a process instance created after
        // it. This rejects stale PPID links after PID reuse. If an exact
        // identity cannot be read, skip the ambiguous branch rather than
        // risking termination of an unrelated process.
        let Ok(child) = ProcessInstance::from_process(*pid, process) else {
            continue;
        };
        if child.start_time < parent.start_time {
            continue;
        }

        visited.insert(*pid);
        descendants.push((child, depth));
        collect_descendants(system, child, depth + 1, visited, descendants);
    }
}

#[cfg(not(windows))]
fn process_start_key(_pid: Pid, process: &Process) -> io::Result<u64> {
    Ok(process.start_time())
}

#[cfg(windows)]
fn process_start_key(pid: Pid, _process: &Process) -> io::Result<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid.as_u32()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let queried =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    let query_error = if queried == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    if let Some(error) = query_error {
        return Err(error);
    }

    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pid_is_a_successful_noop() {
        assert_eq!(kill_tree(u32::MAX, Duration::ZERO).unwrap(), 0);
    }
}
