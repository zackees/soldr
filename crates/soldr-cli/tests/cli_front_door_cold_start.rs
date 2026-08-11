//! soldr#2388 Step 3 — cold-start acceptance: the broker is unconditional, so a
//! front-door `soldr` invocation on a clean isolated root spawns **exactly one**
//! broker, and a second front-door invocation against the same program does NOT
//! spawn a second (the front door is the sole broker-spawner and the broker
//! singleton-binds). Paired with `session_multiprocess_smoke`'s one-daemon
//! assertion, this is the "one build → one broker + one daemon" invariant #2364
//! calls for, exercised on the real process harness (no Docker required).

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::timed_test;

fn unique_program(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("soldr-coldstart-{label}-{:012x}", nanos & 0xFFFF_FFFF_FFFF)
}

/// Read the broker-spawn log under the isolated root.
fn broker_spawn_log(root: &Path) -> String {
    std::fs::read_to_string(root.join("broker-spawn.log")).unwrap_or_default()
}

fn count_substr(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.contains(needle)).count()
}

/// Kill every daemon and broker this test launched under `root`, so nothing
/// leaks past the test. Brokers are `soldr` processes; daemons publish a
/// `daemon.pid`. We can only reliably reap daemons by pid-file here; the broker
/// is reaped by the caller holding its `Child`.
fn stop_daemons_in_root(root: &Path) {
    fn walk(dir: &Path, out: &mut Vec<u32>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n == "daemon.pid") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Some(pid) = text.split_whitespace().next().and_then(|s| s.parse().ok()) {
                        out.push(pid);
                    }
                }
            }
        }
    }
    let mut pids = Vec::new();
    walk(root, &mut pids);
    for pid in pids {
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
        #[cfg(unix)]
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();
    }
}

/// Drain a child stream into a shared buffer so its pipe never blocks.
fn drain<R: std::io::Read + Send + 'static>(reader: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    lines
}

timed_test!(
    front_door_cold_start_spawns_exactly_one_broker,
    Duration::from_secs(90),
    {
        let root = common::unique_temp_dir("coldstart-root");
        let program = unique_program("prog");

        // First front-door invocation on a clean root: it must spawn the broker.
        let mut first = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut first);
        let mut first_child = first
            .arg("status")
            .env("SOLDR_CACHE_DIR", &root)
            .env("SOLDR_BROKER_PROGRAM", &program)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn first front-door soldr status");
        let _o1 = drain(first_child.stdout.take().expect("stdout"));
        let _e1 = drain(first_child.stderr.take().expect("stderr"));
        let _ = first_child.wait().expect("wait first status");

        // The front door waits for the broker to report a bind (or an
        // already-bound refusal) before returning; give it a moment to flush.
        let deadline = Instant::now() + Duration::from_secs(30);
        while count_substr(&broker_spawn_log(&root), "binding at") == 0 && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let binds_after_first = count_substr(&broker_spawn_log(&root), "binding at");

        // Second front-door invocation against the SAME program. It may reuse
        // the live singleton without spawning a duplicate candidate.
        let mut second = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut second);
        let mut second_child = second
            .arg("status")
            .env("SOLDR_CACHE_DIR", &root)
            .env("SOLDR_BROKER_PROGRAM", &program)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn second front-door soldr status");
        let _o2 = drain(second_child.stdout.take().expect("stdout"));
        let _e2 = drain(second_child.stderr.take().expect("stderr"));
        let second_status = second_child.wait().expect("wait second status");
        std::thread::sleep(Duration::from_millis(500));
        let log = broker_spawn_log(&root);
        let total_binds = count_substr(&log, "binding at");

        // Cleanup before asserting so a failure never leaks processes.
        stop_daemons_in_root(&root);

        assert!(
            binds_after_first >= 1,
            "the first front-door invocation on a clean root must bring up a \
             broker (no 'binding at' line in the spawn log)\n{log}"
        );
        assert!(
            second_status.success(),
            "the second front-door command must reuse the live broker\n{log}"
        );
        assert_eq!(
            total_binds, 1,
            "exactly one broker may bind one program; the front door may reuse \
             it without spawning a duplicate candidate\n{log}"
        );
    }
);
