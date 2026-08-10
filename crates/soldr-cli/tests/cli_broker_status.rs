//! soldr#2442 slice 2 — `soldr broker status` queries the running broker over
//! its control socket and prints an admin STATUS snapshot; with no broker
//! bound it prints a clean "not running" line and exits 0 so scripts can probe
//! without starting anything.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod common;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_POLL_BUDGET: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

fn unique_program(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    format!("soldr-broker-status-{label}-{:010x}", nanos & 0xFF_FFFF_FFFF)
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

/// Run `soldr broker status --program <program>` once and return (stdout+stderr,
/// exit code).
fn run_status(program: &str) -> (String, i32) {
    let out = Command::new(common::soldr_bin())
        .args(["broker", "status", "--program", program])
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
    broker_status_reports_not_running_when_no_broker,
    Duration::from_secs(30),
    {
        // A program namespace no broker is serving: status must not error, must
        // say "not running", and must exit 0 (a safe probe).
        let program = unique_program("absent");
        let (output, code) = run_status(&program);
        assert_eq!(code, 0, "status against no broker must exit 0; got:\n{output}");
        assert!(
            output.contains("not running"),
            "status against no broker must report 'not running'; got:\n{output}"
        );
    }
);

soldr_cli::timed_test!(
    broker_status_reports_snapshot_from_running_broker,
    Duration::from_secs(90),
    {
        let program = unique_program("live");
        let mut broker = spawn_broker(&program);
        assert!(
            wait_until_bound(&mut broker, Instant::now() + READY_TIMEOUT),
            "broker never printed its bound-at line within {READY_TIMEOUT:?}"
        );

        // The control socket binds just after the "binding at" line, so poll the
        // status query until the admin round-trip lands (or the budget expires).
        let deadline = Instant::now() + STATUS_POLL_BUDGET;
        let mut last = String::new();
        let ok = loop {
            let (output, code) = run_status(&program);
            last = output;
            if code == 0 && last.contains("broker_instance:") {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
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
);
