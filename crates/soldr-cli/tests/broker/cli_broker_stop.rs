//! `soldr broker stop` terminates only the stable broker, using its self-reported
//! PID (never a process-name sweep). Daemon routes remain alive for re-adoption;
//! with no broker bound the command prints "not running" and exits 0.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::common;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_BUDGET: Duration = Duration::from_secs(20);
const STOP_EXIT_BUDGET: Duration = Duration::from_secs(20);
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

fn run_broker(verb: &str, home: &Path) -> (String, i32) {
    let out = common::isolated_soldr_command()
        .args(["broker", verb])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("run soldr broker {verb}: {e}"));
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.code().unwrap_or(-1))
}

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
fn broker_stop_reports_not_running_when_no_broker() {
    let home = common::unique_temp_dir("broker-stop-absent-home");
    let (output, code) = run_broker("stop", &home);
    assert_eq!(
        code, 0,
        "stop against no broker must exit 0; got:\n{output}"
    );
    assert!(
        output.contains("not running"),
        "stop against no broker must report 'not running'; got:\n{output}"
    );
}

#[test]
fn broker_stop_terminates_running_broker() {
    let home = common::unique_temp_dir("broker-stop-live-home");
    let mut broker = spawn_broker(&home);
    assert!(
        wait_until_bound(&mut broker, Instant::now() + READY_TIMEOUT),
        "broker never printed its bound-at line within {READY_TIMEOUT:?}"
    );

    // Wait for the stable endpoint to actually answer (it binds just after
    // the readiness line) so stop has a live broker to snapshot.
    let status_deadline = Instant::now() + STATUS_POLL_BUDGET;
    loop {
        let (out, code) = run_broker("status", &home);
        if code == 0 && out.contains("broker_instance:") {
            break;
        }
        assert!(
            Instant::now() < status_deadline,
            "stable broker endpoint never answered status; last:\n{out}"
        );
        std::thread::sleep(POLL);
    }

    let (stop_out, stop_code) = run_broker("stop", &home);
    assert_eq!(stop_code, 0, "stop must exit 0; got:\n{stop_out}");
    assert!(
        stop_out.contains("stopped"),
        "stop must confirm it stopped the broker; got:\n{stop_out}"
    );
    // soldr#2442 Option B: a current broker supports the SHUTDOWN verb, so
    // stop must take the cooperative-drain path (not verified-PID fallback).
    assert!(
        stop_out.contains("cooperative shutdown"),
        "stop against a current broker must use cooperative shutdown; got:\n{stop_out}"
    );
    // soldr#3158: and the cooperative path must actually *complete*. Requesting
    // the drain and then force-killing the broker when it never exits is not a
    // cooperative shutdown -- it is the failure the deadline exists to catch,
    // and asserting only on the request line above let it pass unnoticed on
    // every CI run.
    assert!(
        !stop_out.contains("did not exit within the stop deadline"),
        "the cooperative drain must complete, not time out into a force-kill; got:\n{stop_out}"
    );

    // The spawned broker process must actually exit.
    let exit_deadline = Instant::now() + STOP_EXIT_BUDGET;
    let exited = loop {
        if matches!(broker.try_wait(), Ok(Some(_))) {
            break true;
        }
        if Instant::now() >= exit_deadline {
            break false;
        }
        std::thread::sleep(POLL);
    };
    if !exited {
        let _ = broker.kill();
        let _ = broker.wait();
        panic!("broker process did not exit within {STOP_EXIT_BUDGET:?} after `broker stop`");
    }

    // A second status must now report the broker is gone.
    let (after, after_code) = run_broker("status", &home);
    assert_eq!(after_code, 0, "post-stop status must exit 0; got:\n{after}");
    assert!(
        after.contains("not running"),
        "after stop, status must report 'not running'; got:\n{after}"
    );
}

/// soldr#3158: the cooperative drain must complete *quickly*, and the broker
/// must exit of its own accord.
///
/// Every broker stop used to burn the full 10s drain deadline and then
/// SIGKILL: the broker fanned one `tokio::sync::Notify` out to three loops
/// (accept, route reaper, RSS watchdog) but signalled it with `notify_one()`,
/// which wakes exactly one waiter. The reaper registered first and swallowed
/// every SHUTDOWN, so the accept loop that owns the exit never heard it.
///
/// Nothing about that looked like an error -- the request was accepted and the
/// broker did stop -- which is why this asserts on the *shape* of the stop
/// (fast, self-exited) rather than only on its outcome. The deadline is
/// shortened well below the default so a regression fails on the clock instead
/// of being absorbed by a generous budget.
#[test]
fn broker_stop_drains_cooperatively_without_hitting_the_deadline() {
    /// Far below the 10s default: a broker that ignores the request cannot
    /// finish inside this, while a working drain needs milliseconds.
    const DRAIN_DEADLINE_MS: u64 = 2_000;
    /// Generous enough for a loaded CI runner, still far under the deadline.
    const COOPERATIVE_BUDGET: Duration = Duration::from_millis(1_500);

    let home = common::unique_temp_dir("broker-stop-cooperative-home");
    let mut broker = spawn_broker(&home);
    assert!(
        wait_until_bound(&mut broker, Instant::now() + READY_TIMEOUT),
        "broker never printed its bound-at line within {READY_TIMEOUT:?}"
    );

    let status_deadline = Instant::now() + STATUS_POLL_BUDGET;
    loop {
        let (out, code) = run_broker("status", &home);
        if code == 0 && out.contains("broker_instance:") {
            break;
        }
        assert!(
            Instant::now() < status_deadline,
            "stable broker endpoint never answered status; last:\n{out}"
        );
        std::thread::sleep(POLL);
    }

    let started = Instant::now();
    let out = common::isolated_soldr_command()
        .args(["broker", "stop"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            "SOLDR_BROKER_DRAIN_DEADLINE_MS",
            DRAIN_DEADLINE_MS.to_string(),
        )
        .stdin(Stdio::null())
        .output()
        .expect("run soldr broker stop");
    let elapsed = started.elapsed();
    let stop_out = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        out.status.code().unwrap_or(-1),
        0,
        "stop must exit 0; got:\n{stop_out}"
    );
    assert!(
        !stop_out.contains("did not exit within the stop deadline"),
        "stop must not force-kill a broker that was asked to drain; got:\n{stop_out}"
    );
    assert!(
        elapsed < COOPERATIVE_BUDGET,
        "cooperative stop took {elapsed:?}, over the {COOPERATIVE_BUDGET:?} budget \
         (drain deadline was {DRAIN_DEADLINE_MS}ms) -- the broker is not observing \
         the shutdown request; got:\n{stop_out}"
    );

    // The broker must have exited on its own. A force-kill would show up as a
    // signal here even if the stop command's own wording ever changed.
    let exit_deadline = Instant::now() + STOP_EXIT_BUDGET;
    let status = loop {
        match broker.try_wait() {
            Ok(Some(status)) => break status,
            _ => {
                if Instant::now() >= exit_deadline {
                    let _ = broker.kill();
                    let _ = broker.wait();
                    panic!(
                        "broker did not exit within {STOP_EXIT_BUDGET:?}; stop said:\n{stop_out}"
                    );
                }
                std::thread::sleep(POLL);
            }
        }
    };
    assert!(
        status.success(),
        "a cooperatively drained broker must exit 0, not die of a signal; got {status:?} \
         after:\n{stop_out}"
    );
}
