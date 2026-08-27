//! Issue #1814 — two soldr-daemons must never coexist against one root.
//!
//! The reliability audit that produced zccache#1223 observed **two live
//! `soldr-daemon` processes** on a dev host. That is the root availability
//! failure the whole meta hangs off: two daemons both open `~/.soldr/state.sqlite3`,
//! so one loses the exclusive redb file lock, its embedded compile service
//! reports `NotRunning`, and the wrapper falls back into the read-only
//! hardlinked-artifact collision that surfaced as `.rmeta is not writeable`.
//!
//! Three layers are supposed to prevent that (all in
//! `soldr-daemon/src/daemon/{lifecycle,server}.rs`):
//!
//! 1. `RootOwnershipGuard` — an `flock`'d `<cache>/soldr-daemon/root-owner.lock`
//!    held for the daemon's whole lifetime, deliberately version-blind.
//! 2. `existing_daemon_pid` — a route-claim + endpoint-status identity check.
//! 3. `claim_unix_endpoint` — an immediate bind, before init, to close the
//!    accept race.
//!
//! Every existing test covers those in isolation (a threaded spawn-lock race, a
//! single-process `try_acquire` probe, an upper bound on `"event":"spawn"`
//! lines). None spawns two real daemon *processes* and asserts that exactly one
//! survives — which is the property that actually failed in the field. This
//! file closes that gap.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;

/// Readiness budget. Generous because a cold embedded-zccache init has been
/// measured at ~25 s in Docker; the assertions below are about *which* daemon
/// wins, never about how fast it gets there.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the loser is given to notice it lost and exit.
const LOSER_EXIT_TIMEOUT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(100);

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn soldr_daemon_bin() -> PathBuf {
    let soldr = common::soldr_bin();
    let parent = soldr.parent().expect("CARGO_BIN_EXE_soldr has a parent");
    let stem = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn direct_sock(root: &Path) -> PathBuf {
    common::isolated_daemon::isolated_daemon_control_endpoint(&soldr_daemon_bin(), root)
}

/// Spawn a foreground daemon against `cache_root`, capturing its output so the
/// loser's diagnostic can be asserted on.
fn spawn_daemon(cache_root: &Path, home_root: &Path) -> std::process::Child {
    let mut cmd = common::isolated_daemon::isolated_daemon_command(&soldr_daemon_bin(), cache_root);
    cmd.args(["--foreground", "--idle-timeout-secs", "120"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env_remove("RUSTC_WRAPPER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn().expect("spawn soldr-daemon")
}

/// How long to keep asking a daemon for status before concluding it stopped
/// serving.
///
/// A single-shot `status()` can return `WouldBlock` (macOS `EAGAIN`, errno 35)
/// against a perfectly healthy daemon when the runner is loaded -- observed on
/// `target-run x86_64-apple-darwin`, where the assertion read that transient as
/// "the incumbent was displaced".
///
/// This cannot mask a genuinely dead or displaced daemon: the properties under
/// test are that the incumbent keeps serving and keeps its pid, and neither
/// becomes true by waiting. A daemon that lost the endpoint stays lost.
const STATUS_SETTLE_BUDGET: Duration = Duration::from_secs(5);

/// Retry `op` over [`STATUS_SETTLE_BUDGET`] while it fails with a transient
/// socket condition. Any other error returns immediately -- a refused
/// connection, or a protocol mismatch, is an answer rather than an absence of
/// one.
///
/// Generic over the operation so the test never has to name the daemon's
/// private `StatusInfo`; widening a production type's visibility for a test's
/// convenience would be the wrong trade.
fn settled<T>(
    mut op: impl FnMut() -> Result<T, soldr_cli::daemon::client::ClientError>,
) -> Result<T, String> {
    let deadline = Instant::now() + STATUS_SETTLE_BUDGET;
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) => {
                let transient = matches!(
                    &err,
                    soldr_cli::daemon::client::ClientError::Io(io)
                        if matches!(
                            io.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::TimedOut
                        )
                );
                if !transient || Instant::now() >= deadline {
                    return Err(format!("{err:?}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn route_claim(cache_root: &Path) -> PathBuf {
    cache_root
        .join("cache")
        .join("soldr-daemon")
        .join("broker-route-claim.pb")
}

/// Wait until the daemon at `cache_root` is serving, or the deadline passes.
fn wait_until_serving(cache_root: &Path, deadline: Instant) -> bool {
    let sock = direct_sock(cache_root);
    while Instant::now() < deadline {
        if route_claim(cache_root).exists() && soldr_cli::daemon::client::status(&sock).is_ok() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

fn stop_daemon(cache_root: &Path, child: &mut std::process::Child) {
    let _ = soldr_cli::daemon::client::shutdown(&direct_sock(cache_root));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn two_daemons_against_one_root_never_coexist() {
    let cache_root = unique_temp_dir("single-instance-cache");
    let home_root = unique_temp_dir("single-instance-home");

    // First daemon wins the root and starts serving.
    let mut first = spawn_daemon(&cache_root, &home_root);
    assert!(
        wait_until_serving(&cache_root, Instant::now() + READY_TIMEOUT),
        "first daemon never became ready"
    );

    // Record who is serving *before* the challenger appears. Compared
    // against `first.id()` this is immune to the daemon re-execing or
    // relocating itself (which it does on Windows), while still proving the
    // incumbent was not displaced.
    let sock = direct_sock(&cache_root);
    let incumbent_pid = settled(|| soldr_cli::daemon::client::status(&sock))
        .expect("incumbent must be serving")
        .pid;

    // Second daemon, same root. It must refuse rather than coexist.
    let second = spawn_daemon(&cache_root, &home_root);
    let output = {
        let deadline = Instant::now() + LOSER_EXIT_TIMEOUT;
        let mut second = second;
        loop {
            if matches!(second.try_wait(), Ok(Some(_))) {
                break second.wait_with_output().expect("collect loser output");
            }
            if Instant::now() >= deadline {
                let _ = second.kill();
                let _ = second.wait();
                stop_daemon(&cache_root, &mut first);
                panic!(
                    "a second soldr-daemon stayed alive against the same root for \
                         {LOSER_EXIT_TIMEOUT:?} — the single-instance guard did not hold \
                         (issue #1814)"
                );
            }
            std::thread::sleep(POLL);
        }
    };

    // The loser reports why. `daemon_entry` deliberately exits 0 on
    // AlreadyRunning (it is not an error for a redundant spawn to no-op),
    // so the diagnostic is the assertion, not the exit code.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("already running") || combined.contains("root ownership is busy"),
        "second daemon exited without explaining that a daemon already owned the \
             root; output was:\n{combined}"
    );

    // The winner is still the one serving — the loser must not have
    // displaced it, stolen the endpoint, or corrupted the route claim.
    let status = settled(|| soldr_cli::daemon::client::status(&sock))
        .expect("the original daemon must still be serving after the loser exits");
    assert_eq!(
        status.pid, incumbent_pid,
        "the endpoint was served by pid {incumbent_pid} before the second daemon \
             started and by pid {} after — the challenger displaced the incumbent \
             instead of backing off (issue #1814)",
        status.pid
    );

    stop_daemon(&cache_root, &mut first);
}

#[test]
fn losing_daemon_leaves_the_state_db_openable() {
    // The failure mode #1814 is really about: a coexisting second daemon
    // holds `state.sqlite3`, so the incumbent's own opens start failing with
    // `DatabaseAlreadyOpen`. With the guard holding, a rejected second
    // daemon must leave the state DB exactly as openable as before.
    let cache_root = unique_temp_dir("single-instance-db-cache");
    let home_root = unique_temp_dir("single-instance-db-home");

    let mut first = spawn_daemon(&cache_root, &home_root);
    assert!(
        wait_until_serving(&cache_root, Instant::now() + READY_TIMEOUT),
        "first daemon never became ready"
    );

    let mut second = spawn_daemon(&cache_root, &home_root);
    let deadline = Instant::now() + LOSER_EXIT_TIMEOUT;
    while Instant::now() < deadline && !matches!(second.try_wait(), Ok(Some(_))) {
        std::thread::sleep(POLL);
    }
    let _ = second.kill();
    let _ = second.wait();

    // The incumbent still answers, which is only possible if its own
    // state-DB access never lost the file lock to the rejected daemon.
    let sock = direct_sock(&cache_root);
    assert!(
        settled(|| soldr_cli::daemon::client::status(&sock)).is_ok(),
        "incumbent daemon stopped serving after a second daemon was rejected"
    );

    // And no contention was ever recorded against the state DB. This is the
    // #1814 acceptance criterion ("zero DatabaseAlreadyOpen errors") applied
    // to the two-daemon scenario specifically.
    let contention_log = cache_root.join("logs").join("redb-contention.jsonl");
    let contention = std::fs::read_to_string(&contention_log).unwrap_or_default();
    assert!(
        !contention.contains("budget-exhausted"),
        "a rejected second daemon must not cause state-DB open failures; \
             {} contained:\n{contention}",
        contention_log.display()
    );

    stop_daemon(&cache_root, &mut first);
}
