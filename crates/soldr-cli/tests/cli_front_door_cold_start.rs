//! soldr#2388 Step 3 — cold-start acceptance: the broker is unconditional, so a
//! front-door `soldr` invocation on a clean isolated root spawns **exactly one**
//! broker, and a second front-door invocation against the same endpoint does NOT
//! spawn a second (the front door is the sole broker-spawner and the broker
//! singleton-binds). This covers the broker half of the "one build → one
//! broker + one daemon" invariant #2364 calls for, exercised on the real
//! process harness (no Docker required).

mod common;

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use soldr_cli::core::SoldrPaths;

/// Read the broker-spawn log under the isolated home installation.
fn broker_spawn_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".soldr/broker/broker-spawn.log")).unwrap_or_default()
}

fn isolate_home(command: &mut Command, home: &Path) {
    command.env("HOME", home).env("USERPROFILE", home);
}

fn count_substr(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.contains(needle)).count()
}

/// Kill the daemon this test launched for `root`, so nothing leaks past the
/// test. The broker is reaped separately through its stable control endpoint.
fn stop_daemons_in_root(root: &Path) {
    let pid = soldr_cli::daemon::backend_handle_adoption::read_broker_route_claim(
        &SoldrPaths::with_root(root.to_path_buf()),
    )
    .ok()
    .flatten()
    .map(|claim| claim.pid);
    if let Some(pid) = pid {
        soldr_platform::process::terminate::terminate_pid(pid);
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

#[test]
fn front_door_cold_start_spawns_exactly_one_broker() {
    let root = common::unique_temp_dir("coldstart-root");
    let home = common::unique_temp_dir("coldstart-home");
    // First front-door invocation on a clean root: it must spawn the broker.
    let mut first = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut first);
    isolate_home(&mut first, &home);
    let mut first_child = first
        .arg("status")
        .env("SOLDR_CACHE_DIR", &root)
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
    while count_substr(&broker_spawn_log(&home), "stable endpoint bound at") == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    let binds_after_first = count_substr(&broker_spawn_log(&home), "stable endpoint bound at");

    // Second front-door invocation against the same endpoint. It may reuse
    // the live singleton without spawning a duplicate candidate.
    let mut second = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut second);
    isolate_home(&mut second, &home);
    let mut second_child = second
        .arg("status")
        .env("SOLDR_CACHE_DIR", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn second front-door soldr status");
    let _o2 = drain(second_child.stdout.take().expect("stdout"));
    let _e2 = drain(second_child.stderr.take().expect("stderr"));
    let second_status = second_child.wait().expect("wait second status");
    std::thread::sleep(Duration::from_millis(500));
    let log = broker_spawn_log(&home);
    let total_binds = count_substr(&log, "stable endpoint bound at");

    // Cleanup before asserting so a failure never leaks processes.
    let mut stop = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut stop);
    isolate_home(&mut stop, &home);
    let _ = stop.args(["broker", "stop"]).output();
    stop_daemons_in_root(&root);

    assert!(
        binds_after_first >= 1,
        "the first front-door invocation on a clean root must bring up a \
             broker (no stable bind line in the spawn log)\n{log}"
    );
    assert!(
        second_status.success(),
        "the second front-door command must reuse the live broker\n{log}"
    );
    assert_eq!(
        total_binds, 1,
        "exactly one broker may bind the stable endpoint; the front door may reuse \
             it without spawning a duplicate candidate\n{log}"
    );
}
