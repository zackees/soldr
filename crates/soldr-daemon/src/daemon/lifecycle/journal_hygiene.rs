//! Journal hygiene for the daemon lifecycle log (soldr#2436 phase 2):
//! the rotation bound and successor-side unclean-shutdown detection.
//! Split from `mod.rs` for the #2493 1,000-line production ceiling.

use super::*;

/// Rotation rule (soldr#2436 D1): the lifecycle journal is append-only
/// diagnostics, so bound it at daemon start — over 10,000 lines truncate
/// to the newest 5,000. Best-effort: a failed rotation never blocks
/// startup.
const LIFECYCLE_ROTATE_THRESHOLD: usize = 10_000;
const LIFECYCLE_ROTATE_KEEP: usize = 5_000;

pub fn rotate_lifecycle_journal(paths: &SoldrPaths) {
    let path = daemon_lifecycle_log_path(paths);
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= LIFECYCLE_ROTATE_THRESHOLD {
        return;
    }
    let keep = &lines[lines.len() - LIFECYCLE_ROTATE_KEEP..];
    let mut rotated = keep.join("\n");
    rotated.push('\n');
    let _ = fs::write(&path, rotated);
}

/// Successor-side unclean-shutdown detection (soldr#2436 D3): at daemon
/// start, a pid file naming a dead process that never wrote an exit
/// record means the previous daemon died un-drained (SIGKILL, OOM,
/// TerminateProcess) — exactly the transitions that lose in-memory
/// compile contexts. Record it so restart forensics stop dead-ending.
pub fn detect_unclean_shutdown(paths: &SoldrPaths) {
    // Modern daemons publish a broker route claim; releases ≤0.8.29 wrote
    // the two-line daemon.pid. Either artifact naming a dead pid counts.
    let claimed_pid = crate::daemon::backend_handle_adoption::read_broker_route_claim(paths)
        .ok()
        .flatten()
        .map(|claim| claim.pid);
    let legacy_pid = read_legacy_daemon_pid_identity(paths).map(|(pid, _)| pid);
    let Some(dead_pid) = claimed_pid.or(legacy_pid) else {
        return;
    };
    if pid_is_alive(dead_pid) {
        return;
    }
    let journal = fs::read_to_string(daemon_lifecycle_log_path(paths)).unwrap_or_default();
    let has_exit_record = journal.lines().any(|line| {
        line.contains(&format!("\"pid\":{dead_pid},"))
            && (line.contains("\"event\":\"died-") || line.contains("\"event\":\"exit\""))
    });
    if has_exit_record {
        return;
    }
    append_lifecycle_event_with(
        paths,
        "unclean-shutdown-detected",
        LifecycleDetails {
            target_pid: Some(dead_pid),
            ..LifecycleDetails::recording_daemon_identity()
        },
    );
}
