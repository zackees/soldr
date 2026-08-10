//! soldr#2442 slice 2 — `soldr broker stop` terminates the running broker and
//! reaps the daemon routes it owns, using the broker's self-reported PIDs (never
//! by process name). With no broker bound it prints "not running" and exits 0.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_BUDGET: Duration = Duration::from_secs(20);
const STOP_EXIT_BUDGET: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

fn unique_program(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    format!("soldr-broker-stop-{label}-{:010x}", nanos & 0xFF_FFFF_FFFF)
}

fn spawn_broker(program: &str) -> std::process::Child {
    Command::new(common::soldr_bin())
        .args(["broker", "serve", "--program", program])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn soldr broker serve")
}

fn run_broker(program: &str, verb: &str) -> (String, i32) {
    let out = Command::new(common::soldr_bin())
        .args(["broker", verb, "--program", program])
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
            if line.contains("binding at") {
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

soldr_cli::timed_test!(
    broker_stop_reports_not_running_when_no_broker,
    Duration::from_secs(30),
    {
        let program = unique_program("absent");
        let (output, code) = run_broker(&program, "stop");
        assert_eq!(code, 0, "stop against no broker must exit 0; got:\n{output}");
        assert!(
            output.contains("not running"),
            "stop against no broker must report 'not running'; got:\n{output}"
        );
    }
);

soldr_cli::timed_test!(
    broker_stop_terminates_running_broker,
    Duration::from_secs(120),
    {
        let program = unique_program("live");
        let mut broker = spawn_broker(&program);
        assert!(
            wait_until_bound(&mut broker, Instant::now() + READY_TIMEOUT),
            "broker never printed its bound-at line within {READY_TIMEOUT:?}"
        );

        // Wait for the control socket to actually answer (it binds just after
        // the readiness line) so stop has a live broker to snapshot.
        let status_deadline = Instant::now() + STATUS_POLL_BUDGET;
        loop {
            let (out, code) = run_broker(&program, "status");
            if code == 0 && out.contains("broker_instance:") {
                break;
            }
            assert!(
                Instant::now() < status_deadline,
                "broker control socket never answered status; last:\n{out}"
            );
            std::thread::sleep(POLL);
        }

        let (stop_out, stop_code) = run_broker(&program, "stop");
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
        let (after, after_code) = run_broker(&program, "status");
        assert_eq!(after_code, 0, "post-stop status must exit 0; got:\n{after}");
        assert!(
            after.contains("not running"),
            "after stop, status must report 'not running'; got:\n{after}"
        );
    }
);
