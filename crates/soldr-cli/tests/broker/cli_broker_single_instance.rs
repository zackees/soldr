//! Two brokers for the same installed Soldr endpoint must never coexist.
//!
//! Mirrors `cli_daemon_single_instance.rs`'s shape for the analogous
//! `soldr-daemon` property, applied to the new broker subcommand. `soldr
//! broker serve` binds via `running_process::broker::server::singleton_bind`
//! (running-process#899/#901): the first process to bind wins, and a second
//! process against the same bind name must be refused rather than racing it.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const LOSER_EXIT_TIMEOUT: Duration = Duration::from_secs(15);
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

/// Wait until `child`'s stdout has printed the "binding at" line, or the
/// deadline passes. Consumes stdout on a background thread so a live
/// process's inherited pipe never fills up and blocks it.
fn wait_until_bound(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::thread::JoinHandle<bool>> {
    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take()?;
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("stable endpoint bound at") {
                return true;
            }
        }
        false
    });
    // Poll-join with a deadline rather than a blocking join, since a wedged
    // process would otherwise hang the test past its own nextest budget.
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            return Some(handle);
        }
        if Instant::now() >= deadline {
            return None;
        }
        if start.elapsed() > READY_TIMEOUT {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

#[test]
fn two_brokers_against_one_endpoint_never_coexist() {
    let home = common::unique_temp_dir("broker-single-home");

    let mut first = spawn_broker(&home);
    let ready = wait_until_bound(&mut first, Instant::now() + READY_TIMEOUT);
    assert!(
        ready.is_some(),
        "first broker never printed its bound-at line within {READY_TIMEOUT:?}"
    );

    // A second broker for the same installed endpoint must refuse.
    let second = spawn_broker(&home);
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
                let _ = first.kill();
                let _ = first.wait();
                panic!(
                    "a second broker stayed alive for {LOSER_EXIT_TIMEOUT:?} -- the \
                         singleton guard did not hold"
                );
            }
            std::thread::sleep(POLL);
        }
    };

    assert_eq!(
        output.status.code(),
        Some(75),
        "loser broker must exit EX_TEMPFAIL(75), got {:?}",
        output.status
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("another broker is already bound"),
        "second broker exited without explaining that another broker already \
             owned the bind path; output was:\n{combined}"
    );
    // soldr#2024 exit-guard regression check: an explained non-zero exit
    // must not ALSO get the generic "fault in soldr itself" annotation
    // (this is the mark_spoke() bug this test would have caught).
    assert!(
        !combined.contains("fault in soldr itself"),
        "the already-bound refusal is a real explanation and must not trip \
             the silent-failure annotation; output was:\n{combined}"
    );

    let _ = first.kill();
    let _ = first.wait();
}
