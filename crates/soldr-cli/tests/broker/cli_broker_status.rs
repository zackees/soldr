//! soldr#2442 slice 2 — `soldr broker status` queries the running broker over
//! its control socket and prints an admin STATUS snapshot; with no broker
//! bound it prints a clean "not running" line and exits 0 so scripts can probe
//! without starting anything.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_BUDGET: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

fn spawn_broker(home: &Path) -> std::process::Child {
    common::isolated_soldr_command()
        .args(["broker", "serve"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn soldr broker serve")
}

/// Run `soldr broker status` once and return (stdout+stderr, exit code).
fn run_status(home: &Path) -> (String, i32) {
    let out = common::isolated_soldr_command()
        .args(["broker", "status"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .expect("run soldr broker status");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.code().unwrap_or(-1))
}

/// Consume `child`'s stdout on a background thread until it prints "binding at"
/// or the deadline passes.
fn wait_until_bound(child: &mut std::process::Child, deadline: Instant) -> bool {
    use std::io::{BufRead, BufReader};
    let Some(stdout) = child.stdout.take() else {
        return false;
    };
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("stable endpoint bound at") {
                return true;
            }
        }
        false
    });
    loop {
        if handle.is_finished() {
            return handle.join().unwrap_or(false);
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

#[test]
fn broker_status_reports_not_running_when_no_broker() {
    // With no broker serving, status must remain a successful probe.
    let home = common::unique_temp_dir("broker-status-absent-home");
    let (output, code) = run_status(&home);
    assert_eq!(
        code, 0,
        "status against no broker must exit 0; got:\n{output}"
    );
    assert!(
        output.contains("not running"),
        "status against no broker must report 'not running'; got:\n{output}"
    );
}

#[test]
fn broker_status_reports_snapshot_from_running_broker() {
    let home = common::unique_temp_dir("broker-status-live-home");
    let mut broker = spawn_broker(&home);
    assert!(
        wait_until_bound(&mut broker, Instant::now() + READY_TIMEOUT),
        "broker never printed its bound-at line within {READY_TIMEOUT:?}"
    );

    // The control socket binds just after the "binding at" line, so poll the
    // status query until the admin round-trip lands (or the budget expires).
    let deadline = Instant::now() + STATUS_POLL_BUDGET;
    let (ok, last) = loop {
        let (output, code) = run_status(&home);
        if code == 0 && output.contains("broker_instance:") {
            break (true, output);
        }
        if Instant::now() >= deadline {
            break (false, output);
        }
        std::thread::sleep(POLL);
    };

    let _ = broker.kill();
    let _ = broker.wait();

    assert!(
        ok,
        "status never returned a live snapshot within {STATUS_POLL_BUDGET:?}; \
             last output was:\n{last}"
    );
    assert!(
        last.contains("accepting_hello:"),
        "live status snapshot missing accepting_hello; got:\n{last}"
    );
}
