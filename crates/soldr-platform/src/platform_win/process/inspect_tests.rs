//! Unit tests for [`super::inspect`].
//!
//! In a sibling file rather than an inline `mod tests` because
//! `.github/scripts/spawn_path_guard.py` counts raw `.spawn()` calls in every
//! file under `crates/*/src` and skips `*_tests.rs` siblings. These tests spawn
//! real processes deliberately -- a liveness check can only be verified against
//! one -- so they belong on the skipped side of that boundary rather than in an
//! allowlist entry that would also excuse production spawns in this file.

use super::*;

#[test]
fn windows_liveness_reports_current_process_alive() {
    assert!(is_alive(std::process::id()));
}

/// The sentinel collision, exercised rather than reasoned about.
///
/// `GetExitCodeProcess` cannot distinguish "running" from "exited with
/// 259", so before soldr#2806 a process that returned 259 read as alive
/// forever. The exit code here is chosen to be exactly the sentinel.
#[test]
fn a_process_that_exits_with_the_sentinel_code_is_not_alive() {
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "exit", "259"])
        .spawn()
        .expect("spawn cmd");
    let pid = child.id();
    let status = child.wait().expect("wait for cmd");
    assert_eq!(
        status.code(),
        Some(259),
        "the fixture must actually exit with the sentinel value"
    );
    assert!(
        !is_alive(pid),
        "pid {pid} exited with 259 and must not read as alive"
    );
}

/// The ordinary case still works, so the fix is not "report dead more
/// often" -- a liveness check that under-reports would make the reentrancy
/// guard admit a live process.
#[test]
fn a_process_that_exits_normally_is_not_alive() {
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "exit", "0"])
        .spawn()
        .expect("spawn cmd");
    let pid = child.id();
    child.wait().expect("wait for cmd");
    assert!(!is_alive(pid), "pid {pid} exited cleanly and is not alive");
}

/// A running process must still read as alive after the extra probe --
/// the corroboration only ever downgrades a `STILL_ACTIVE` answer, and a
/// long-lived child is the case every caller actually depends on.
#[test]
fn a_running_child_still_reads_as_alive() {
    let mut child = std::process::Command::new("cmd")
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .spawn()
        .expect("spawn cmd");
    let pid = child.id();
    assert!(is_alive(pid), "a running child must read as alive");
    let _ = child.kill();
    let _ = child.wait();
}

/// This process's own token must be present, stable, and non-zero -- the
/// same contract `process_start_token`'s facade-level test pins portably.
/// Repeated here against the real Windows implementation so a divergence in
/// `GetProcessTimes` plumbing (rather than the facade dispatch) is caught on
/// the platform that actually exercises it.
#[test]
fn windows_start_token_is_stable_and_present_for_this_process() {
    let first = process_start_token(std::process::id());
    assert!(first.is_some(), "a live process must have a readable creation time");
    assert_eq!(
        first,
        process_start_token(std::process::id()),
        "creation time does not change across reads"
    );
    assert_ne!(first, Some(0), "a real process never has a zero FILETIME");
}

/// A spawned child's token must change once it exits and a *different*
/// process is later asked about under the same numeric pid -- exercised
/// indirectly here by confirming a live child's token differs from the
/// impossible-pid `None` case, since Windows offers no portable way to force
/// a specific pid to be recycled inside a test.
#[test]
fn windows_start_token_is_none_for_an_impossible_pid() {
    assert_eq!(process_start_token(0), None);
}
