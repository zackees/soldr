//! Zombie reaper — background task that periodically scans the process registry
//! for dead, zombie, and orphan processes and cleans them up.

use std::sync::Arc;

use sysinfo::{Pid, ProcessRefreshKind, Signal, System};
use tracing::{debug, info, warn};

use crate::daemon::handlers::DaemonState;
use crate::daemon::registry::TrackedEntry;

// ---------------------------------------------------------------------------
// Classification types
// ---------------------------------------------------------------------------

/// How a tracked process was classified during a reaper scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessClassification {
    /// Process is alive and matches its registered creation time.
    Healthy,
    /// Process is gone from the OS — it exited without being unregistered.
    Dead(String),
    /// Process exists but its creation time does not match — the PID was reused.
    Zombie(String),
    /// Process exists but its parent is no longer alive.
    Orphan(String),
}

/// Information about a zombie/dead/orphan process found by the reaper.
#[derive(Debug, Clone)]
pub struct ZombieInfo {
    pub pid: u32,
    pub command: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Classification logic
// ---------------------------------------------------------------------------

/// Classify a tracked registry entry against the current OS state.
///
/// `system` must have already been refreshed for the entry's PID before calling
/// this function.
pub fn classify_process(entry: &TrackedEntry, system: &System) -> ProcessClassification {
    let sysinfo_pid = Pid::from_u32(entry.pid);

    let Some(proc) = system.process(sysinfo_pid) else {
        return ProcessClassification::Dead(format!("process {} no longer exists", entry.pid));
    };

    // Check creation time with 2-second tolerance (same as registry recovery).
    let proc_start_ms = proc.start_time() * 1000;
    if proc_start_ms.abs_diff(entry.created_at_ms) > 2000 {
        return ProcessClassification::Zombie(format!(
            "PID {} reused: registered creation_time={}ms, OS creation_time={}ms",
            entry.pid, entry.created_at_ms, proc_start_ms
        ));
    }

    // Check parent liveness.
    if let Some(parent_pid) = proc.parent() {
        if system.process(parent_pid).is_none() {
            return ProcessClassification::Orphan(format!(
                "parent PID {} of process {} is dead",
                parent_pid.as_u32(),
                entry.pid
            ));
        }
    }

    ProcessClassification::Healthy
}

// ---------------------------------------------------------------------------
// Scan & kill
// ---------------------------------------------------------------------------

/// Scan the registry for zombie/dead/orphan processes.
///
/// Returns a list of [`ZombieInfo`] for each non-healthy entry.
pub fn scan_for_zombies(state: &DaemonState) -> Vec<ZombieInfo> {
    let entries = state.registry.list_all();
    if entries.is_empty() {
        return Vec::new();
    }

    let mut system = System::new();
    // Refresh only the PIDs we care about.
    for entry in &entries {
        let sysinfo_pid = Pid::from_u32(entry.pid);
        system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());
        // Also refresh parent if known, so we can check parent liveness.
        if let Some(proc) = system.process(sysinfo_pid) {
            if let Some(parent_pid) = proc.parent() {
                system.refresh_process_specifics(parent_pid, ProcessRefreshKind::new());
            }
        }
    }

    let mut zombies = Vec::new();
    for entry in &entries {
        match classify_process(entry, &system) {
            ProcessClassification::Healthy => {}
            ProcessClassification::Dead(reason)
            | ProcessClassification::Zombie(reason)
            | ProcessClassification::Orphan(reason) => {
                zombies.push(ZombieInfo {
                    pid: entry.pid,
                    command: entry.command.clone(),
                    reason,
                });
            }
        }
    }

    zombies
}

/// Scan for orphaned conhost.exe processes system-wide.
///
/// These are conhost.exe instances whose parent process has died — typically
/// leftovers from ConPTY sessions that were not properly cleaned up.
/// Unlike registry-based zombie scanning, this uses a Toolhelp process snapshot
/// and does not require prior registration.
#[cfg(windows)]
pub fn scan_for_orphan_conhosts() -> Vec<ZombieInfo> {
    crate::pty::find_orphan_conhosts()
        .into_iter()
        .map(|c| ZombieInfo {
            pid: c.pid,
            command: "conhost.exe".to_string(),
            reason: format!("orphan conhost.exe — parent PID {} is dead", c.parent_pid),
        })
        .collect()
}

/// No-op on non-Windows platforms.
#[cfg(not(windows))]
pub fn scan_for_orphan_conhosts() -> Vec<ZombieInfo> {
    Vec::new()
}

/// Kill the given zombie processes and unregister them from the registry.
///
/// Returns a vec of `(pid, killed)` tuples indicating whether each process was
/// successfully killed.  Dead processes that no longer exist are still
/// unregistered and report `killed = true`.
pub fn kill_zombies(state: &DaemonState, zombies: &[ZombieInfo]) -> Vec<(u32, bool)> {
    let mut system = System::new();
    let mut results = Vec::with_capacity(zombies.len());

    for z in zombies {
        let sysinfo_pid = Pid::from_u32(z.pid);
        system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

        let killed = match system.process(sysinfo_pid) {
            Some(proc) => {
                let result = proc.kill_with(Signal::Kill).unwrap_or(false);
                if result {
                    info!(pid = z.pid, "killed zombie process");
                } else {
                    warn!(pid = z.pid, "failed to kill zombie process");
                }
                result
            }
            None => {
                // Already dead — still counts as success for cleanup purposes.
                debug!(pid = z.pid, "zombie process already dead, unregistering");
                true
            }
        };

        // Unregister from the registry regardless.
        state.registry.unregister(z.pid);
        results.push((z.pid, killed));
    }

    results
}

/// Kill orphaned conhost.exe processes (not in the registry, so no unregister needed).
///
/// Returns a vec of `(pid, killed)` tuples.
pub fn kill_conhosts(conhosts: &[ZombieInfo]) -> Vec<(u32, bool)> {
    let mut system = System::new();
    let mut results = Vec::with_capacity(conhosts.len());

    for z in conhosts {
        let sysinfo_pid = Pid::from_u32(z.pid);
        system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

        let killed = match system.process(sysinfo_pid) {
            Some(proc) => {
                let result = proc.kill_with(Signal::Kill).unwrap_or(false);
                if result {
                    info!(pid = z.pid, "killed orphan conhost.exe");
                } else {
                    warn!(pid = z.pid, "failed to kill orphan conhost.exe");
                }
                result
            }
            None => {
                debug!(pid = z.pid, "orphan conhost.exe already dead");
                true
            }
        };
        results.push((z.pid, killed));
    }

    results
}

// ---------------------------------------------------------------------------
// Background reaper loop
// ---------------------------------------------------------------------------

/// Long-running async task that periodically scans for and kills zombie processes.
///
/// Runs until the daemon's shutdown signal is received.
pub async fn reaper_loop(state: Arc<DaemonState>, interval_secs: u64) {
    let mut shutdown_rx = state.shutdown_tx.subscribe();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));

    // The first tick fires immediately; consume it so we don't scan on startup.
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // sysinfo calls are blocking, so run on a blocking thread.
                let scan_state = Arc::clone(&state);
                let result = tokio::task::spawn_blocking(move || {
                    // Registry-based zombie scan.
                    let zombies = scan_for_zombies(&scan_state);
                    if !zombies.is_empty() {
                        info!("reaper scan: found {} zombie(s)", zombies.len());
                        let results = kill_zombies(&scan_state, &zombies);
                        for (z, (_pid, killed)) in zombies.iter().zip(results.iter()) {
                            info!(
                                pid = z.pid,
                                killed = killed,
                                reason = %z.reason,
                                "reaper: processed zombie"
                            );
                        }
                    }

                    // Orphan conhost.exe scan (Windows ConPTY cleanup).
                    let orphan_conhosts = scan_for_orphan_conhosts();
                    if !orphan_conhosts.is_empty() {
                        info!("reaper scan: found {} orphan conhost(s)", orphan_conhosts.len());
                        let results = kill_conhosts(&orphan_conhosts);
                        for (z, (_pid, killed)) in orphan_conhosts.iter().zip(results.iter()) {
                            info!(
                                pid = z.pid,
                                killed = killed,
                                reason = %z.reason,
                                "reaper: processed orphan conhost"
                            );
                        }
                    }

                    if zombies.is_empty() && orphan_conhosts.is_empty() {
                        debug!("reaper scan: no zombies or orphan conhosts found");
                    }
                })
                .await;

                if let Err(e) = result {
                    warn!("reaper task panicked: {e}");
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("reaper shutting down");
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::{Registry, TrackedEntry};
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::watch;

    /// Build a minimal `DaemonState` for testing.
    fn test_state() -> (DaemonState, tempfile::TempDir) {
        let (shutdown_tx, _rx) = watch::channel(false);
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let db_path = tmp_dir.path().join("test-reaper.db");
        let registry = Arc::new(Registry::open(&db_path).unwrap());
        let pty_sessions = Arc::new(crate::daemon::pty_sessions::PtySessionRegistry::new());
        let pipe_sessions = Arc::new(crate::daemon::pipe_sessions::PipeSessionRegistry::new());
        let services = Arc::new(
            crate::daemon::services::ServiceRegistry::open(
                &db_path,
                tmp_dir.path().join("services"),
            )
            .unwrap(),
        );
        let state = DaemonState {
            start_time: Instant::now(),
            version: "0.0.0-test".to_string(),
            socket_path: "/tmp/test.sock".to_string(),
            db_path: "/tmp/test.db".to_string(),
            scope: "global".to_string(),
            scope_hash: "0000000000000000".to_string(),
            scope_cwd: "/tmp".to_string(),
            shutdown_tx,
            active_connections: AtomicU32::new(0),
            registry,
            pty_sessions,
            pipe_sessions,
            services,
            emergency_reserve: Arc::new(
                crate::daemon::emergency_reserve::EmergencyReserve::initialize_at(
                    tmp_dir.path().join("emergency-reserve.bin"),
                    4096,
                ),
            ),
        };
        (state, tmp_dir)
    }

    #[test]
    fn scan_empty_registry_returns_empty() {
        let (state, _tmp) = test_state();
        let zombies = scan_for_zombies(&state);
        assert!(zombies.is_empty(), "expected no zombies in empty registry");
    }

    #[test]
    fn scan_detects_dead_process() {
        let (state, _tmp) = test_state();

        // Register a fake PID that certainly does not exist.
        let entry = TrackedEntry {
            pid: 4_000_000,
            created_at_ms: 1_000_000,
            kind: "subprocess".to_string(),
            command: "fake-dead-process".to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:reaper".to_string(),
            containment: "contained".to_string(),
            registered_at: 1000.0,
        };
        state.registry.register(entry).unwrap();

        let zombies = scan_for_zombies(&state);
        assert_eq!(zombies.len(), 1, "expected 1 zombie for dead fake PID");
        assert_eq!(zombies[0].pid, 4_000_000);
        assert!(zombies[0].reason.contains("no longer exists"));
    }

    #[test]
    fn classify_healthy_current_process() {
        let pid = std::process::id();
        let mut system = System::new();
        let sysinfo_pid = Pid::from_u32(pid);
        system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

        let proc_start_ms = system
            .process(sysinfo_pid)
            .map(|p| p.start_time() * 1000)
            .unwrap_or(0);

        // Also refresh the parent so orphan check works.
        if let Some(proc) = system.process(sysinfo_pid) {
            if let Some(parent_pid) = proc.parent() {
                system.refresh_process_specifics(parent_pid, ProcessRefreshKind::new());
            }
        }

        let entry = TrackedEntry {
            pid,
            created_at_ms: proc_start_ms,
            kind: "subprocess".to_string(),
            command: "self".to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:reaper".to_string(),
            containment: "contained".to_string(),
            registered_at: 1000.0,
        };

        let classification = classify_process(&entry, &system);
        assert_eq!(classification, ProcessClassification::Healthy);
    }

    #[test]
    fn classify_zombie_wrong_creation_time() {
        let pid = std::process::id();
        let mut system = System::new();
        let sysinfo_pid = Pid::from_u32(pid);
        system.refresh_process_specifics(sysinfo_pid, ProcessRefreshKind::new());

        // Use a wildly wrong creation time so it is classified as Zombie (PID reuse).
        let entry = TrackedEntry {
            pid,
            created_at_ms: 1, // wrong creation time
            kind: "subprocess".to_string(),
            command: "self".to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:reaper".to_string(),
            containment: "contained".to_string(),
            registered_at: 1000.0,
        };

        let classification = classify_process(&entry, &system);
        assert!(
            matches!(classification, ProcessClassification::Zombie(_)),
            "expected Zombie classification for wrong creation time, got {:?}",
            classification
        );
    }

    #[test]
    fn classify_dead_process() {
        let system = System::new();
        // PID 4_000_000 is very unlikely to exist.
        let entry = TrackedEntry {
            pid: 4_000_000,
            created_at_ms: 1_000_000,
            kind: "subprocess".to_string(),
            command: "dead".to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:reaper".to_string(),
            containment: "contained".to_string(),
            registered_at: 1000.0,
        };

        let classification = classify_process(&entry, &system);
        assert!(
            matches!(classification, ProcessClassification::Dead(_)),
            "expected Dead classification for non-existent PID, got {:?}",
            classification
        );
    }

    #[test]
    fn kill_zombies_unregisters_dead_entries() {
        let (state, _tmp) = test_state();

        // Register a dead fake process.
        let entry = TrackedEntry {
            pid: 4_000_001,
            created_at_ms: 1_000_000,
            kind: "subprocess".to_string(),
            command: "dead-to-kill".to_string(),
            cwd: "/tmp".to_string(),
            originator: "test:reaper".to_string(),
            containment: "contained".to_string(),
            registered_at: 1000.0,
        };
        state.registry.register(entry).unwrap();
        assert_eq!(state.registry.count(), 1);

        let zombies = scan_for_zombies(&state);
        assert_eq!(zombies.len(), 1);

        let results = kill_zombies(&state, &zombies);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 4_000_001);
        assert!(results[0].1, "dead process should report killed=true");

        // Registry should now be empty.
        assert_eq!(state.registry.count(), 0);
    }
}
