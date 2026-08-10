//! #539 slice 7 — macOS descendant-lifecycle backend.
//!
//! **History:** the first cut of this module used `kqueue` +
//! `EVFILT_PROC` + `NOTE_TRACK`. Empirically on the macos-arm CI
//! runner, NOTE_TRACK silently failed to emit `NOTE_CHILD` events
//! for spawned descendants — a long-standing reliability issue with
//! NOTE_TRACK on modern macOS that Apple has not addressed (the
//! recommended replacement is Endpoint Security, which requires the
//! `com.apple.developer.endpoint-security.client` entitlement and is
//! out of scope for the no-admin LaunchedProcessTree tier). After
//! the integration test failed twice with `got 0 (all: [])` despite
//! synchronous registration before the spawn race window, we pivoted
//! to the same polling shape Linux uses.
//!
//! **Current implementation:** snapshot every process on the system
//! via `sysctl({CTL_KERN, KERN_PROC, KERN_PROC_ALL})` every 50 ms,
//! build a parent → children map, BFS from the root PID, diff
//! against the previous snapshot, and emit
//! [`DescendantStarted`](crate::observer::ObserverEventKind::DescendantStarted)
//! / [`DescendantExited`](crate::observer::ObserverEventKind::DescendantExited)
//! on the consumer's [`ObserverSubscriber`].
//!
//! Tradeoffs vs. Endpoint Security:
//!
//! - **No entitlement required.** Works against any process the
//!   calling user owns.
//! - **Polling-based**: short-lived descendants that spawn and exit
//!   within the same 50 ms window may be missed. Same honesty caveat
//!   as the Linux `/proc` poll.
//! - **Per-snapshot cost**: one `sysctl()` walk of the full process
//!   table. Typically a few hundred entries; cheap.

#![cfg(target_os = "macos")]

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::observer::pid_identity;
use crate::observer::{DescendantPumpStop, EventCategory, ObserverEvent, ObserverEventKind};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

// libc 0.2 exposes the `proc_listpids` / `proc_pidinfo` functions and
// the `proc_bsdinfo` struct on macOS targets but does NOT export the
// integer constants below, so we declare them inline. The values are
// from Apple's `<sys/proc_info.h>` and have been ABI-stable for years.
const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: libc::c_int = 3;

/// Spawn the descendant-tracking pump thread for `root_pid`. Returns
/// silently after spawning — the thread terminates when `root_pid`
/// disappears from the global process table or its creation-time identity
/// changes.
pub(crate) fn spawn_pump(
    root_pid: u32,
    sink: Sender<ObserverEvent>,
    stop: Arc<DescendantPumpStop>,
) {
    let Some(root_identity) = process_info(root_pid).map(|info| info.identity) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("rp-macos-descpump".to_string())
        .spawn(move || pump_loop(root_pid, root_identity, sink, stop));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    start_sec: u64,
    start_usec: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    identity: ProcessIdentity,
}

fn pump_loop(
    root_pid: u32,
    root_identity: ProcessIdentity,
    sink: Sender<ObserverEvent>,
    stop: Arc<DescendantPumpStop>,
) {
    pump_loop_with(
        &stop,
        sink,
        || descendant_snapshot(root_pid, root_identity),
        || stop.wait_timeout(POLL_INTERVAL),
    );
}

fn pump_loop_with(
    stop: &DescendantPumpStop,
    sink: Sender<ObserverEvent>,
    mut snapshot: impl FnMut() -> Option<HashSet<u32>>,
    mut wait: impl FnMut() -> bool,
) {
    let mut known: HashSet<u32> = HashSet::new();
    loop {
        if stop.is_stopped() {
            return;
        }
        let Some(current) = snapshot() else {
            // Root is gone — emit exits for any remaining tracked
            // descendants and terminate. Mirrors the Linux pump's
            // /proc-disappearance termination condition.
            for &pid in &known {
                let _ = sink.send(ObserverEvent::new_now(
                    EventCategory::Process,
                    ObserverEventKind::DescendantExited,
                    pid,
                ));
            }
            break;
        };
        emit_diff(&known, &current, &sink);
        known = current;
        if wait() {
            return;
        }
    }
}

/// Snapshot every process on the system, returning a `Vec<(pid, ppid)>`.
///
/// Uses `proc_listpids(PROC_ALL_PIDS)` to enumerate PIDs, then
/// `proc_pidinfo(pid, PROC_PIDTBSDINFO)` to look up each PPID. This
/// avoids depending on `libc::kinfo_proc` (which our pinned libc
/// 0.2 does not export on macOS targets) and is the documented
/// Apple API for cross-process introspection.
fn list_all_processes() -> Vec<ProcessInfo> {
    // proc_listpids size probe — pass null buffer to learn the
    // required size in bytes.
    let size = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return Vec::new();
    }
    let pid_count = (size as usize) / std::mem::size_of::<libc::pid_t>();
    if pid_count == 0 {
        return Vec::new();
    }
    let mut pids: Vec<libc::pid_t> = vec![0; pid_count];
    let written_bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut libc::c_void,
            (pid_count * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if written_bytes <= 0 {
        return Vec::new();
    }
    let written = (written_bytes as usize) / std::mem::size_of::<libc::pid_t>();
    pids.truncate(written);

    let mut result = Vec::with_capacity(written);
    for &pid in &pids {
        if pid <= 0 {
            continue;
        }
        if let Some(info) = process_info(pid as u32) {
            result.push(info);
        }
    }
    result
}

fn process_info(pid: u32) -> Option<ProcessInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    (n as usize == std::mem::size_of::<libc::proc_bsdinfo>()).then_some(ProcessInfo {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        identity: ProcessIdentity {
            // Use the complete timeval: seconds alone can alias when a busy
            // host recycles a PID within the same second.
            start_sec: info.pbi_start_tvsec,
            start_usec: info.pbi_start_tvusec,
        },
    })
}

#[cfg(test)]
fn descendants_if_root_matches(
    root_pid: u32,
    expected_identity: ProcessIdentity,
    all: &[ProcessInfo],
) -> Option<HashSet<u32>> {
    all.iter()
        .any(|info| {
            info.pid == root_pid && pid_identity::matches(&expected_identity, Some(&info.identity))
        })
        .then(|| descendants_of(root_pid, all))
}

fn descendant_snapshot(root_pid: u32, expected_identity: ProcessIdentity) -> Option<HashSet<u32>> {
    let identity_before = process_info(root_pid).map(|info| info.identity);
    if !pid_identity::matches(&expected_identity, identity_before.as_ref()) {
        return None;
    }
    let descendants = descendants_of(root_pid, &list_all_processes());
    verified_snapshot(
        expected_identity,
        identity_before,
        descendants,
        process_info(root_pid).map(|info| info.identity),
    )
}

fn verified_snapshot(
    expected_identity: ProcessIdentity,
    identity_before: Option<ProcessIdentity>,
    descendants: HashSet<u32>,
    identity_after: Option<ProcessIdentity>,
) -> Option<HashSet<u32>> {
    (pid_identity::matches(&expected_identity, identity_before.as_ref())
        && pid_identity::matches(&expected_identity, identity_after.as_ref()))
    .then_some(descendants)
}

/// BFS the descendant subtree of `root_pid` given the full
/// `(pid, ppid)` snapshot. Returns the set of every transitive
/// descendant (the root itself is excluded).
fn descendants_of(root_pid: u32, all: &[ProcessInfo]) -> HashSet<u32> {
    let mut child_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for info in all {
        child_map.entry(info.ppid).or_default().push(info.pid);
    }
    let mut result = HashSet::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Some(children) = child_map.get(&pid) {
            for &c in children {
                if result.insert(c) {
                    stack.push(c);
                }
            }
        }
    }
    result
}

/// Emit DescendantStarted for `current \ prev` and DescendantExited
/// for `prev \ current`. Send errors are ignored — a dropped
/// subscriber must never crash the pump.
fn emit_diff(prev: &HashSet<u32>, current: &HashSet<u32>, sink: &Sender<ObserverEvent>) {
    for &new_pid in current.difference(prev) {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantStarted,
            new_pid,
        ));
    }
    for &gone_pid in prev.difference(current) {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantExited,
            gone_pid,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn process(pid: u32, ppid: u32, start_usec: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            identity: ProcessIdentity {
                start_sec: 100,
                start_usec,
            },
        }
    }

    #[test]
    fn emit_diff_fires_one_started_per_new_pid() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20].into_iter().collect();
        let current: HashSet<u32> = [10, 20, 30, 40].into_iter().collect();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        let evs: Vec<_> = rx.iter().collect();
        assert_eq!(evs.len(), 2);
        let started_pids: HashSet<u32> = evs
            .iter()
            .filter(|e| matches!(e.kind, ObserverEventKind::DescendantStarted))
            .map(|e| e.pid)
            .collect();
        assert_eq!(started_pids, [30, 40].into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn emit_diff_fires_one_exited_per_gone_pid() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20, 30].into_iter().collect();
        let current: HashSet<u32> = [10].into_iter().collect();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        let evs: Vec<_> = rx.iter().collect();
        assert_eq!(evs.len(), 2);
        let exited_pids: HashSet<u32> = evs
            .iter()
            .filter(|e| matches!(e.kind, ObserverEventKind::DescendantExited))
            .map(|e| e.pid)
            .collect();
        assert_eq!(exited_pids, [20, 30].into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn descendants_of_handles_branching_tree() {
        // Tree: 100 -> {200, 300}; 200 -> {201}; 300 has no children.
        let all = vec![
            process(100, 0, 1),
            process(200, 100, 2),
            process(201, 200, 3),
            process(300, 100, 4),
            process(999, 1, 5),
        ];
        let descendants = descendants_of(100, &all);
        assert_eq!(
            descendants,
            [200, 201, 300].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn descendants_of_for_unknown_root_returns_empty() {
        let all = vec![process(100, 0, 1), process(200, 100, 2)];
        let descendants = descendants_of(0x7FFF_FFFE, &all);
        assert!(descendants.is_empty());
    }

    #[test]
    fn list_all_processes_returns_non_empty_on_real_macos() {
        // Sanity check the sysctl pipeline on the actual macos-arm
        // CI runner — there's always at least `launchd`, the test
        // process itself, plus dozens of system daemons.
        let all = list_all_processes();
        assert!(
            all.len() > 5,
            "expected the macOS process table to have plenty of entries, got {}",
            all.len()
        );
        // The current process must be in there.
        let self_pid = std::process::id();
        assert!(
            all.iter().any(|info| info.pid == self_pid),
            "expected current pid {self_pid} in process table"
        );
    }

    #[test]
    fn reused_root_pid_identity_mismatch_terminates_snapshot() {
        let expected = ProcessIdentity {
            start_sec: 100,
            start_usec: 1,
        };
        let reused = vec![process(100, 0, 99), process(200, 100, 2)];
        assert_eq!(descendants_if_root_matches(100, expected, &reused), None);
    }

    #[test]
    fn root_identity_change_after_walk_rejects_mixed_snapshot() {
        let expected = ProcessIdentity {
            start_sec: 100,
            start_usec: 1,
        };
        let recycled = ProcessIdentity {
            start_sec: 100,
            start_usec: 99,
        };
        assert_eq!(
            verified_snapshot(
                expected,
                Some(expected),
                [42].into_iter().collect(),
                Some(recycled),
            ),
            None
        );
    }

    #[test]
    fn scripted_normal_pump_emits_descendant_start_and_exit() {
        let (tx, rx) = mpsc::channel();
        let stop = DescendantPumpStop::new();
        let mut snapshots =
            [Some([42].into_iter().collect()), Some(HashSet::new()), None].into_iter();
        pump_loop_with(&stop, tx, || snapshots.next().flatten(), || false);
        let events: Vec<_> = rx.iter().collect();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].kind,
            ObserverEventKind::DescendantStarted
        ));
        assert!(matches!(
            events[1].kind,
            ObserverEventKind::DescendantExited
        ));
        assert!(events.iter().all(|event| event.pid == 42));
    }

    #[test]
    fn stop_wakes_waiting_pump_without_polling() {
        let stop = Arc::new(DescendantPumpStop::new());
        let pump_stop = Arc::clone(&stop);
        let (tx, _rx) = mpsc::channel();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let pump = std::thread::spawn(move || {
            let mut announced = false;
            pump_loop_with(
                &pump_stop,
                tx,
                || {
                    if !announced {
                        announced = true;
                        waiting_tx.send(()).unwrap();
                    }
                    Some(HashSet::new())
                },
                || pump_stop.wait_timeout(Duration::from_secs(30)),
            );
            done_tx.send(()).unwrap();
        });

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        stop.stop();
        done_rx.recv_timeout(Duration::from_millis(250)).unwrap();
        pump.join().unwrap();
    }

    #[test]
    fn end_to_end_descendant_started_and_exited_for_spawned_chain() {
        use crate::observer::ObserverConfig;
        use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

        // Same fixture shape as the Linux integration test. With
        // 50 ms polling and bash totalling ~700 ms, the snapshot
        // diff catches the three background sleeps comfortably.
        let cfg = ProcessConfig {
            command: CommandSpec::Argv(vec![
                "bash".into(),
                "-c".into(),
                "sleep 0.5 & sleep 0.5 & sleep 0.5 & wait".into(),
            ]),
            cwd: None,
            env: None,
            capture: false,
            stderr_mode: StderrMode::Stdout,
            creationflags: None,
            create_process_group: false,
            stdin_mode: StdinMode::Inherit,
            nice: None,
        };
        let (process, subscriber) = NativeProcess::with_observer(
            cfg,
            ObserverConfig::with_categories([EventCategory::Process]),
        );
        process.start().expect("spawn bash chain");
        let _ = process
            .wait(Some(Duration::from_secs(30)))
            .expect("bash chain exits");
        process.close().ok();
        // Give the pump time to run the final diff + emit exits and
        // hit its root-disappeared termination check.
        std::thread::sleep(Duration::from_millis(200));

        let events = subscriber.drain();
        let started = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantStarted)
            })
            .count();
        let exited = events
            .iter()
            .filter(|e| {
                e.category == EventCategory::Process
                    && matches!(e.kind, ObserverEventKind::DescendantExited)
            })
            .count();
        assert!(
            started >= 3,
            "expected ≥3 DescendantStarted, got {started} (all: {events:?})"
        );
        assert!(
            exited >= 3,
            "expected ≥3 DescendantExited, got {exited} (all: {events:?})"
        );
        for ev in &events {
            assert_eq!(
                ev.category,
                EventCategory::Process,
                "Lifecycle leaked into Process-only subscriber: {ev:?}"
            );
        }
    }
}
