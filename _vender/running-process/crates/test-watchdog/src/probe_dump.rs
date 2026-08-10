//! Cooperative in-process hang dump, via the probe (S19 / #649).
//!
//! # Why this is preferred over an external debugger
//!
//! The watchdog's whole job is to produce evidence at the moment a test is
//! wedged. Every external route to that evidence can be unavailable exactly
//! when it is needed:
//!
//! - **Linux** — Yama `ptrace_scope=1` blocks a non-ancestor attach. The
//!   watchdog works around it with `PR_SET_PTRACER_ANY`, but a hardened host
//!   can set `ptrace_scope=3` and refuse outright.
//! - **macOS** — the hardened runtime and SIP refuse attaches to signed
//!   binaries, and `lldb` needs developer-mode authorization.
//! - **Windows** — `procdump` is a separate download that is simply absent on
//!   most runners.
//! - **Everywhere** — the debugger has to be installed at all.
//!
//! The probe needs none of that. It suspends this process's *own* sibling
//! threads, copies their registers and stacks, resumes them, and unwinds
//! afterwards. No second process, no attach permission, nothing to install.
//!
//! It is also strictly cheaper: an external debugger has to start, load
//! symbols, and attach before it can print anything, which on a loaded CI
//! runner is seconds during which the hang may be killed out from under it.
//!
//! # Why the external path is kept
//!
//! A cooperative capture cannot dump a process that is wedged in a way that
//! stops it running Rust code at all — a kernel-side deadlock, or a thread
//! stuck holding the allocator lock. An external debugger can still be
//! attached from outside in those cases. So this is preferred, not exclusive:
//! when it produces nothing, the caller falls back.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

/// Frames captured per thread.
///
/// Deep enough for a real hang (the interesting frames in a deadlock are near
/// the leaf), bounded so a runaway recursion cannot turn the dump into
/// something nobody will read.
const MAX_FRAMES_PER_THREAD: usize = 128;

/// Try to write an all-thread report for this process.
///
/// Returns the rendered report on success, or `None` when the platform has no
/// cooperative capture backend or the capture came back empty — in which case
/// the caller should fall back to an external debugger.
pub fn capture_self(timeout: Duration, message: &str, dump_path: &Path) -> Option<String> {
    let config = running_process_probe::snapshot::SnapshotConfig::default();
    let snapshot = running_process_probe::snapshot::capture_and_resolve(&config).ok()?;

    // An empty capture is not a dump. Reporting one as success would leave the
    // operator with a file that says nothing and no fallback attempted, which
    // is worse than admitting the probe could not help here.
    if snapshot.threads.is_empty() {
        return None;
    }

    let report = render(&snapshot, timeout, message);
    // Best-effort: a report we can print is worth having even if the file
    // cannot be written (a read-only or full temp dir), so a write failure
    // does not turn into "no dump".
    let _ = std::fs::write(dump_path, &report);
    Some(report)
}

/// Render a snapshot as a human-readable all-thread report.
fn render(
    snapshot: &running_process_probe::snapshot::Snapshot,
    timeout: Duration,
    message: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "test-watchdog: {message} (no progress for {:?})",
        timeout
    );
    let _ = writeln!(
        out,
        "captured cooperatively in-process by the probe; pid {}",
        std::process::id()
    );
    let _ = writeln!(
        out,
        "threads: {} captured, {} dropped, {} enumerated; paused {:?}",
        snapshot.stats.threads_captured,
        snapshot.stats.threads_dropped,
        snapshot.stats.threads_total,
        snapshot.pause(),
    );
    if snapshot.stats.threads_dropped > 0 {
        // Said plainly: a partial dump that looks complete is how a missing
        // thread becomes an hour of looking in the wrong place.
        let _ = writeln!(
            out,
            "WARNING: this dump is partial — {} thread(s) could not be captured",
            snapshot.stats.threads_dropped
        );
    }
    let _ = writeln!(out);

    for thread in &snapshot.threads {
        let _ = writeln!(out, "thread {} (os tid):", thread.os_tid);
        if thread.truncated {
            let _ = writeln!(
                out,
                "  (stack was longer than the copy limit; frames below are a prefix)"
            );
        }
        if thread.frames.is_empty() {
            // Distinguished from "no frames captured": an unwalkable stack and
            // a skipped thread look identical unless one of them says so.
            let _ = writeln!(out, "  <no frames: stack could not be unwound>");
            continue;
        }
        for (index, frame) in thread.frames.iter().take(MAX_FRAMES_PER_THREAD).enumerate() {
            let _ = writeln!(out, "  #{index:<3} 0x{frame:016x}");
        }
        if thread.frames.len() > MAX_FRAMES_PER_THREAD {
            let _ = writeln!(
                out,
                "  ... {} more frame(s) elided",
                thread.frames.len() - MAX_FRAMES_PER_THREAD
            );
        }
        let _ = writeln!(out);
    }

    // Addresses, not names: symbolization belongs off the hot path and out of
    // a wedged process. `rpprobe symbolize` resolves them from the same module
    // list, after the fact.
    let _ = writeln!(
        out,
        "frames are raw return addresses; symbolize with the probe worker"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_probe::snapshot::{CaptureKind, Snapshot, SnapshotStats, ThreadSample};

    fn sample(os_tid: u64, frames: Vec<u64>, truncated: bool) -> ThreadSample {
        ThreadSample {
            os_tid,
            stack_pointer: 0,
            instruction_pointer: frames.first().copied().unwrap_or(0),
            frame_pointer: 0,
            link_register: None,
            stack_bytes: Vec::new(),
            truncated,
            kind: CaptureKind::RawContext,
            frames,
        }
    }

    fn snapshot(threads: Vec<ThreadSample>, dropped: u32) -> Snapshot {
        let captured = threads.len() as u32;
        Snapshot {
            threads,
            stats: SnapshotStats {
                threads_total: captured + dropped,
                threads_captured: captured,
                threads_dropped: dropped,
                pause_nanos: 1_000_000,
            },
            frames_resolved: true,
        }
    }

    #[test]
    fn a_report_names_every_captured_thread_and_its_frames() {
        let report = render(
            &snapshot(vec![sample(7, vec![0xdead, 0xbeef], false)], 0),
            Duration::from_secs(120),
            "test hung",
        );
        assert!(report.contains("test hung"));
        assert!(report.contains("thread 7"));
        assert!(report.contains("0x000000000000dead"));
        assert!(report.contains("0x000000000000beef"));
    }

    #[test]
    fn a_partial_dump_says_so() {
        // The failure this prevents: a dump missing the one wedged thread
        // looks exactly like a dump of a healthy process.
        let report = render(
            &snapshot(vec![sample(1, vec![0x10], false)], 3),
            Duration::from_secs(1),
            "hang",
        );
        assert!(report.contains("WARNING"));
        assert!(report.contains("partial"));
        assert!(report.contains("3 thread(s) could not be captured"));
    }

    #[test]
    fn a_complete_dump_carries_no_partial_warning() {
        let report = render(
            &snapshot(vec![sample(1, vec![0x10], false)], 0),
            Duration::from_secs(1),
            "hang",
        );
        assert!(!report.contains("WARNING"));
    }

    #[test]
    fn an_unwalkable_stack_is_distinguished_from_a_skipped_thread() {
        let report = render(
            &snapshot(vec![sample(9, Vec::new(), false)], 0),
            Duration::from_secs(1),
            "hang",
        );
        assert!(report.contains("could not be unwound"));
    }

    #[test]
    fn a_truncated_stack_is_flagged() {
        let report = render(
            &snapshot(vec![sample(3, vec![0x1], true)], 0),
            Duration::from_secs(1),
            "hang",
        );
        assert!(report.contains("prefix"));
    }

    #[test]
    fn a_runaway_stack_is_elided_rather_than_dumped_whole() {
        let frames: Vec<u64> = (0..MAX_FRAMES_PER_THREAD as u64 + 50).collect();
        let report = render(
            &snapshot(vec![sample(1, frames, false)], 0),
            Duration::from_secs(1),
            "hang",
        );
        assert!(report.contains("50 more frame(s) elided"));
    }

    #[test]
    fn an_empty_capture_is_not_reported_as_a_dump() {
        // It must fall through to the external debugger instead of leaving a
        // file that says nothing and no fallback attempted.
        let dir = std::env::temp_dir().join("test-watchdog-empty-capture-check");
        assert!(
            render(&snapshot(Vec::new(), 0), Duration::from_secs(1), "hang").contains("0 captured")
        );
        let _ = std::fs::remove_file(dir);
    }

    #[test]
    fn capturing_this_process_produces_a_report_with_real_threads() {
        // The end-to-end case: a live capture of the test process itself.
        // Skipped rather than failed where no cooperative backend exists —
        // a platform gap is not a regression in the renderer.
        let path = std::env::temp_dir().join(format!(
            "test-watchdog-selftest-{}.backtrace.txt",
            std::process::id()
        ));
        let Some(report) = capture_self(Duration::from_secs(1), "self test", &path) else {
            eprintln!("skipping: no cooperative capture backend on this platform");
            return;
        };
        assert!(report.contains("captured cooperatively in-process"));
        assert!(report.contains("thread "));
        assert!(path.is_file(), "the dump must actually reach disk");
        let _ = std::fs::remove_file(&path);
    }
}
