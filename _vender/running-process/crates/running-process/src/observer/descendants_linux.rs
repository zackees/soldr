//! #539 slice 5 — Linux descendant-lifecycle backend.
//!
//! No-admin Linux primitive: enable `PR_SET_CHILD_SUBREAPER` so orphaned
//! descendants reparent to us (not to init), and run a background pump
//! that snapshots `/proc/<pid>/task/<pid>/children` every 50 ms,
//! diffing against the previous snapshot to emit
//! [`DescendantStarted`](crate::observer::ObserverEventKind::DescendantStarted)
//! / [`DescendantExited`](crate::observer::ObserverEventKind::DescendantExited)
//! on the consumer's [`ObserverSubscriber`].
//!
//! Tradeoffs vs. eBPF / cn_proc:
//!
//! - **No CAP_BPF / no CAP_NET_ADMIN required.** Works on stock kernels
//!   from any non-elevated process.
//! - **Polling-based**: short-lived descendants that spawn and exit
//!   within the same 50 ms window may be missed. This is the same
//!   tradeoff `proc_pidinfo`-based macOS snapshots make and is the only
//!   honest no-admin option on Linux.
//! - **Subreaper is process-wide**: `prctl(PR_SET_CHILD_SUBREAPER, 1)`
//!   affects the whole calling process. Setting it idempotently is
//!   safe; we never clear it.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::observer::pid_identity;
use crate::observer::{DescendantPumpStop, EventCategory, ObserverEvent, ObserverEventKind};

/// Poll interval for the /proc descendant snapshot. 50 ms is the same
/// cadence we'd expect a debug UI to refresh at, and matches the
/// short-lived-descendant honesty caveat in this module's docs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Enable `PR_SET_CHILD_SUBREAPER` so orphaned descendants of any
/// process this process spawns reparent to us instead of init. Safe to
/// call repeatedly — `prctl` is idempotent here.
///
/// Errors are deliberately swallowed: if subreaper can't be set (e.g.
/// inside a sandbox), the pump still works for descendants whose
/// immediate parent stays alive — we just lose long-tail tracking of
/// orphaned descendants. The matrix advertised behavior is still
/// honored.
pub(crate) fn enable_subreaper() {
    // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)` is a leaf
    // syscall with no pointer arguments; cannot violate Rust aliasing.
    let _ = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
}

/// Spawn the descendant-tracking pump thread for `root_pid`. Returns
/// silently after spawning — the thread terminates when `root_pid`
/// exits.
pub(crate) fn spawn_pump(
    root_pid: u32,
    sink: Sender<ObserverEvent>,
    stop: Arc<DescendantPumpStop>,
) {
    let Some(root_identity) = process_identity(root_pid) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("rp-linux-descpump".to_string())
        .spawn(move || pump_loop(root_pid, root_identity, sink, stop));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    start_ticks: u64,
}

fn parse_start_ticks(stat: &str) -> Option<u64> {
    // `comm` is parenthesized but may itself contain spaces and `)`, so split
    // after the final close paren. The suffix begins at field 3 (`state`).
    let suffix = stat.get(stat.rfind(')')? + 1..)?;
    suffix
        .split_ascii_whitespace()
        .nth(19) // field 22 (`starttime`)
        .and_then(|field| field.parse().ok())
}

fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some(ProcessIdentity {
        start_ticks: parse_start_ticks(&stat)?,
    })
}

/// Walk `/proc/<pid>/task/<pid>/children` recursively, returning every
/// transitive descendant PID of `root_pid`. Robust to mid-walk exits:
/// a missing `children` file just truncates that branch of the walk.
fn descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut result = Vec::new();
    let mut stack: Vec<u32> = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let path = format!("/proc/{pid}/task/{pid}/children");
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for token in contents.split_ascii_whitespace() {
            if let Ok(child) = token.parse::<u32>() {
                result.push(child);
                stack.push(child);
            }
        }
    }
    result
}

/// The pump loop. Snapshots descendants every [`POLL_INTERVAL`], diffs
/// against the previous snapshot to emit `DescendantStarted` for new
/// PIDs and `DescendantExited` for missing PIDs. Terminates when the
/// root process is gone, emitting `DescendantExited` for any
/// still-tracked descendants on the way out.
fn descendant_snapshot(root_pid: u32, expected_identity: ProcessIdentity) -> Option<HashSet<u32>> {
    let identity_before = process_identity(root_pid);
    if !pid_identity::matches(&expected_identity, identity_before.as_ref()) {
        return None;
    }
    let descendants = descendant_pids(root_pid).into_iter().collect();
    // Re-read after the recursive walk so a root exit/reuse during the walk
    // cannot publish descendants belonging to the recycled process.
    verified_snapshot(
        expected_identity,
        identity_before,
        descendants,
        process_identity(root_pid),
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
        // Exit when the root is gone — the pump's contract is bounded
        // by the spawned tree's lifetime, mirroring the Windows IOCP
        // pump's ACTIVE_PROCESS_ZERO termination semantics.
        let Some(current) = snapshot() else {
            break;
        };
        emit_diff(&known, &current, &sink);
        known = current;
        if wait() {
            return;
        }
    }
    // Root exited: surface any still-tracked descendants as exited so
    // the consumer's started/exited counts stay balanced.
    for &pid in &known {
        let _ = sink.send(ObserverEvent::new_now(
            EventCategory::Process,
            ObserverEventKind::DescendantExited,
            pid,
        ));
    }
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
    fn emit_diff_no_events_when_steady_state() {
        let (tx, rx) = mpsc::channel();
        let prev: HashSet<u32> = [10, 20].into_iter().collect();
        let current = prev.clone();
        emit_diff(&prev, &current, &tx);
        drop(tx);
        assert_eq!(rx.iter().count(), 0);
    }

    #[test]
    fn descendant_pids_for_nonexistent_root_returns_empty() {
        // /proc/<missing>/task/<missing>/children won't exist — the
        // walk should terminate cleanly with an empty result, not panic.
        let pids = descendant_pids(0x7FFF_FFFE);
        assert!(pids.is_empty(), "expected no descendants, got {pids:?}");
    }

    #[test]
    fn descendant_pids_for_self_includes_no_phantom_entries() {
        // For a process that has no children right now (test thread),
        // the walk returns either an empty list or only well-known
        // children we just spawned. We just assert it doesn't panic
        // and the returned PIDs all look plausible (non-zero).
        let pids = descendant_pids(std::process::id());
        for pid in pids {
            assert!(pid > 1, "pid {pid} is suspiciously small");
        }
    }

    #[test]
    fn parse_start_ticks_handles_spaces_and_parentheses_in_comm() {
        let mut fields = vec!["S".to_string()];
        fields.extend((4..=21).map(|n| n.to_string()));
        fields.push("424242".to_string());
        let stat = format!("123 (odd ) process name) {}", fields.join(" "));
        assert_eq!(parse_start_ticks(&stat), Some(424242));
    }

    #[test]
    fn identity_mismatch_terminates_pump_without_tracking_reused_pid() {
        let (tx, rx) = mpsc::channel();
        let stop = DescendantPumpStop::new();
        let mut polls = 0;
        pump_loop_with(
            &stop,
            tx,
            || {
                polls += 1;
                None
            },
            || panic!("terminated pump must not wait"),
        );
        assert_eq!(polls, 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reused_pid_with_different_start_ticks_rejects_descendant_snapshot() {
        let expected = ProcessIdentity { start_ticks: 100 };
        let recycled = ProcessIdentity { start_ticks: 200 };
        assert_eq!(
            verified_snapshot(
                expected,
                Some(recycled),
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

        // Direct child: bash that spawns 3 sleepers in the background
        // then waits on them. Each background sleep is a descendant
        // (subprocess of bash), so we expect ≥3 DescendantStarted
        // followed by ≥3 DescendantExited as they run to completion.
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

        // The pump terminates once root is gone and flushes pending
        // exits; give it a beat past the poll interval to settle.
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
            "expected ≥3 DescendantStarted events, got {started} (all: {events:?})"
        );
        assert!(
            exited >= 3,
            "expected ≥3 DescendantExited events, got {exited} (all: {events:?})"
        );
        // Only Process events should appear — Lifecycle wasn't requested.
        for ev in &events {
            assert_eq!(
                ev.category,
                EventCategory::Process,
                "Lifecycle leaked into Process-only subscriber: {ev:?}"
            );
        }
    }
}
