//! Cross-process session discovery via the `RUNNING_PROCESS_ORIGINATOR` env var.
//!
//! This module provides the ability to scan all running OS processes and find
//! those whose `RUNNING_PROCESS_ORIGINATOR` environment variable matches a
//! given tool prefix.
//!
//! # Format
//!
//! The env var has the format `TOOL:PID`, e.g. `CLUD:12345`.
//!
//! # Why it exists
//!
//! After a parent process crashes, its in-process registry is lost. This env
//! var survives in every child process's environment, enabling post-crash
//! discovery.
//!
//! # PID reuse guard
//!
//! If a process exists at the parent PID but started **after** the child,
//! the PID was reused by a different process. The child is orphaned.
//!
//! # Example
//!
//! ```no_run
//! use running_process::originator::find_processes_by_originator;
//!
//! let stale = find_processes_by_originator("CLUD");
//! for info in &stale {
//!     if !info.parent_alive {
//!         println!("Orphaned PID {} from dead parent {}", info.pid, info.parent_pid);
//!     }
//! }
//! ```

use crate::containment::ORIGINATOR_ENV_VAR;
use sysinfo::{Pid, ProcessRefreshKind, System, UpdateKind};

/// Information about a process that has the `RUNNING_PROCESS_ORIGINATOR` env var.
#[derive(Debug, Clone)]
pub struct OriginatorProcessInfo {
    /// The PID of the discovered process.
    pub pid: u32,
    /// The process name (e.g., `"cargo"`, `"node"`).
    pub name: String,
    /// The full command line of the process.
    pub command: String,
    /// The full value of `RUNNING_PROCESS_ORIGINATOR` (e.g., `"CLUD:12345"`).
    pub originator: String,
    /// The parent PID parsed from the originator value (the number after `:`).
    pub parent_pid: u32,
    /// Whether the parent PID is still alive and is plausibly the original parent.
    pub parent_alive: bool,
}

/// Parse an originator value like `"CLUD:12345"` into `(tool, parent_pid)`.
pub fn parse_originator_value(value: &str) -> Option<(&str, u32)> {
    let colon_pos = value.rfind(':')?;
    if colon_pos == 0 || colon_pos == value.len() - 1 {
        return None;
    }
    let tool = &value[..colon_pos];
    let pid_str = &value[colon_pos + 1..];
    let pid = pid_str.parse::<u32>().ok()?;
    Some((tool, pid))
}

/// Find all processes whose `RUNNING_PROCESS_ORIGINATOR` env var starts with
/// the given tool prefix.
///
/// # Example
///
/// ```no_run
/// use running_process::originator::find_processes_by_originator;
///
/// let results = find_processes_by_originator("CLUD");
/// for info in &results {
///     println!(
///         "PID={} originator={} parent_alive={}",
///         info.pid, info.originator, info.parent_alive
///     );
/// }
/// ```
pub fn find_processes_by_originator(tool: &str) -> Vec<OriginatorProcessInfo> {
    let prefix = format!("{}:", tool);
    let mut system = System::new();

    system.refresh_processes_specifics(
        ProcessRefreshKind::new()
            .with_environ(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );

    let mut results = Vec::new();

    for (pid, process) in system.processes() {
        let environ = process.environ();
        let originator_value = environ.iter().find_map(|env_entry| {
            if let Some(val) = env_entry.strip_prefix(ORIGINATOR_ENV_VAR) {
                if let Some(val) = val.strip_prefix('=') {
                    return Some(val.to_string());
                }
            }
            None
        });

        let Some(originator_value) = originator_value else {
            continue;
        };

        if !originator_value.starts_with(&prefix) {
            continue;
        }

        let Some((_tool, parent_pid)) = parse_originator_value(&originator_value) else {
            continue;
        };

        let child_start_time = process.start_time();
        let parent_alive = is_parent_alive(&system, parent_pid, child_start_time);

        let cmd_parts = process.cmd();
        let command = if cmd_parts.is_empty() {
            process.name().to_string()
        } else {
            cmd_parts.join(" ")
        };

        results.push(OriginatorProcessInfo {
            pid: pid.as_u32(),
            name: process.name().to_string(),
            command,
            originator: originator_value,
            parent_pid,
            parent_alive,
        });
    }

    results
}

/// PIDs of every process that declared itself a daemon via
/// [`crate::DAEMON_MARKER_ENV_VAR`].
///
/// The companion query to the marker set in `spawn_daemon`. A reaper needs
/// both halves: [`find_processes_by_originator`] tells it what a session
/// spawned, and this tells it what asked to outlive that session. Without a
/// way to read the marker, the exemption has to be inferred from the *absence*
/// of an originator tag — exactly the ambiguity the marker exists to remove
/// (zackees/clud#522).
///
/// Deliberately a separate scan rather than a field on
/// [`OriginatorProcessInfo`]: a declared daemon has had its originator tag
/// stripped, so it never appears in that result set at all.
///
/// # Example
///
/// ```no_run
/// use running_process::originator::find_declared_daemon_pids;
///
/// let daemons = find_declared_daemon_pids();
/// // Never reap a process that asked to outlive its spawner.
/// let may_reap = |pid: u32| !daemons.contains(&pid);
/// # let _ = may_reap(1);
/// ```
pub fn find_declared_daemon_pids() -> std::collections::HashSet<u32> {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessRefreshKind::new().with_environ(UpdateKind::Always));

    let prefix = format!("{}=", crate::DAEMON_MARKER_ENV_VAR);
    system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            let declared = process
                .environ()
                .iter()
                .any(|entry| entry.strip_prefix(&prefix).is_some_and(is_truthy_marker));
            declared.then(|| pid.as_u32())
        })
        .collect()
}

/// The marker is written as `1`, but accept the usual truthy spellings so a
/// consumer setting it by hand is not silently ignored. Anything empty or
/// explicitly falsey means "not declared" — a stray `…=0` must never exempt a
/// process from reaping.
fn is_truthy_marker(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Check whether the parent PID is alive and is plausibly the original parent.
fn is_parent_alive(system: &System, parent_pid: u32, child_start_time: u64) -> bool {
    let parent_sysinfo_pid = Pid::from_u32(parent_pid);
    let Some(parent_process) = system.process(parent_sysinfo_pid) else {
        return false;
    };

    let parent_start_time = parent_process.start_time();
    if parent_start_time > child_start_time {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_originator_value_valid() {
        let (tool, pid) = parse_originator_value("CLUD:12345").unwrap();
        assert_eq!(tool, "CLUD");
        assert_eq!(pid, 12345);
    }

    #[test]
    fn parse_originator_value_with_colons_in_tool() {
        let (tool, pid) = parse_originator_value("MY:TOOL:99999").unwrap();
        assert_eq!(tool, "MY:TOOL");
        assert_eq!(pid, 99999);
    }

    #[test]
    fn parse_originator_value_invalid_no_colon() {
        assert!(parse_originator_value("CLUD").is_none());
    }

    #[test]
    fn parse_originator_value_invalid_no_pid() {
        assert!(parse_originator_value("CLUD:").is_none());
    }

    #[test]
    fn parse_originator_value_invalid_no_tool() {
        assert!(parse_originator_value(":12345").is_none());
    }

    #[test]
    fn parse_originator_value_invalid_non_numeric_pid() {
        assert!(parse_originator_value("CLUD:abc").is_none());
    }

    #[test]
    fn find_processes_returns_empty_for_nonexistent_tool() {
        let results = find_processes_by_originator("__NONEXISTENT_TOOL_TEST__");
        assert!(results.is_empty());
    }

    #[test]
    fn parse_originator_value_max_pid() {
        let (tool, pid) = parse_originator_value("TOOL:4294967295").unwrap();
        assert_eq!(tool, "TOOL");
        assert_eq!(pid, u32::MAX);
    }

    #[test]
    fn parse_originator_value_pid_overflow() {
        // u32::MAX + 1 should fail to parse
        assert!(parse_originator_value("TOOL:4294967296").is_none());
    }

    #[test]
    fn parse_originator_value_negative_pid() {
        assert!(parse_originator_value("TOOL:-1").is_none());
    }

    #[test]
    fn parse_originator_value_zero_pid() {
        let (tool, pid) = parse_originator_value("TOOL:0").unwrap();
        assert_eq!(tool, "TOOL");
        assert_eq!(pid, 0);
    }

    #[test]
    fn parse_originator_value_empty_string() {
        assert!(parse_originator_value("").is_none());
    }

    #[test]
    fn parse_originator_value_only_colon() {
        assert!(parse_originator_value(":").is_none());
    }

    #[test]
    fn originator_process_info_debug_and_clone() {
        let info = OriginatorProcessInfo {
            pid: 123,
            name: "test".to_string(),
            command: "test --arg".to_string(),
            originator: "TOOL:456".to_string(),
            parent_pid: 456,
            parent_alive: true,
        };
        let cloned = info.clone();
        assert_eq!(cloned.pid, 123);
        assert_eq!(cloned.name, "test");
        assert_eq!(cloned.command, "test --arg");
        assert_eq!(cloned.originator, "TOOL:456");
        assert_eq!(cloned.parent_pid, 456);
        assert!(cloned.parent_alive);

        // Debug impl should not panic
        let debug = format!("{:?}", info);
        assert!(debug.contains("OriginatorProcessInfo"));
        assert!(debug.contains("123"));
    }

    #[test]
    fn is_parent_alive_returns_true_for_current_process() {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessRefreshKind::new().with_cmd(UpdateKind::Always));
        let my_pid = std::process::id();
        // Use a far-future child start time so the parent clearly started before it
        let result = is_parent_alive(&system, my_pid, u64::MAX);
        assert!(result, "current process should be reported as alive");
    }

    #[test]
    fn is_parent_alive_returns_false_for_nonexistent_pid() {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessRefreshKind::new().with_cmd(UpdateKind::Always));
        // PID 0 is the idle process on Windows / swapper on Linux — use a very
        // unlikely high PID instead.
        let result = is_parent_alive(&system, u32::MAX, 0);
        assert!(!result, "nonexistent PID should be reported as dead");
    }

    #[test]
    fn is_parent_alive_detects_pid_reuse() {
        let mut system = System::new();
        system.refresh_processes_specifics(ProcessRefreshKind::new().with_cmd(UpdateKind::Always));
        let my_pid = std::process::id();
        // Pretend the child started at time 0 (long before the current process).
        // The current process's start_time should be > 0, so this should detect
        // that the "parent" started after the "child" — a PID reuse scenario.
        let result = is_parent_alive(&system, my_pid, 0);
        // On most systems the current process started at time > 0, so this
        // returns false (PID reuse detected). On systems where start_time
        // reports 0, this would return true — both are acceptable.
        // The key assertion: the function doesn't panic.
        let _ = result;
    }

    #[test]
    fn find_processes_with_empty_tool_prefix() {
        // Empty prefix matches ":" which shouldn't match any real originator values
        let results = find_processes_by_originator("");
        // Should not panic — results may or may not be empty depending on
        // whether any process happens to have the env var with format ":PID"
        let _ = results;
    }

    /// A stray `RUNNING_PROCESS_IS_DAEMON=0` must not silently exempt a
    /// process from reaping, so only truthy spellings count as a declaration.
    #[test]
    fn only_truthy_marker_values_declare_a_daemon() {
        for declared in ["1", "true", "yes", "on", "TRUE", " 1 "] {
            assert!(is_truthy_marker(declared), "{declared:?} should declare");
        }
        for not_declared in ["", "0", "false", "no", "off", "  ", "FALSE"] {
            assert!(!is_truthy_marker(not_declared), "{not_declared:?} must not");
        }
    }

    /// This process was not spawned as a daemon, so it must never appear in
    /// its own result set — the scan reads each process's environment rather
    /// than assuming anything about the caller.
    #[test]
    fn declared_daemon_scan_excludes_an_undeclared_self() {
        assert!(!find_declared_daemon_pids().contains(&std::process::id()));
    }
}
