//! soldr#2388 Step 3 — cold-start acceptance: the broker is unconditional, so
//! concurrent front-door `soldr` invocations on a clean isolated root elect
//! **exactly one** broker starter through the SQLite WAL lease. Every contender
//! derives the same install-path-scoped pipe, and none falls back to an
//! uncoordinated spawn. Paired with `session_multiprocess_smoke`'s one-daemon
//! assertion, this is the "one build → one broker + one daemon" invariant #2364
//! calls for, exercised on the real process harness (no Docker required).

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
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

const CONTENDER_COUNT: usize = 8;

/// Kill every daemon this test launched under `root`, so nothing leaks past the
/// test. The broker is stopped separately through its path-derived control
/// pipe; daemons also publish a `daemon.pid` for this cleanup backstop.
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

fn stop_broker(root: &Path, program: &str) {
    let mut stop = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut stop);
    let _ = stop
        .args(["broker", "stop", "--program", program])
        .env("SOLDR_CACHE_DIR", root)
        .env("SOLDR_BROKER_DRAIN_DEADLINE_MS", "1000")
        .output();
}

timed_test!(
    front_door_cold_start_spawns_exactly_one_broker,
    Duration::from_secs(90),
    {
        let root = common::unique_temp_dir("coldstart-root");
        let program = unique_program("prog");

        // Start all contenders before waiting on any one of them. Each process
        // owns a separate SQLite connection; one takes the short write lease
        // while the rest remain optimistic WAL readers and poll the exact pipe.
        let mut contenders = Vec::with_capacity(CONTENDER_COUNT);
        for _ in 0..CONTENDER_COUNT {
            let mut command = Command::new(common::soldr_bin());
            common::scrub_outer_soldr_env(&mut command);
            contenders.push(
                command
                    .arg("status")
                    .env("SOLDR_CACHE_DIR", &root)
                    .env("SOLDR_BROKER_PROGRAM", &program)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn concurrent front-door soldr status"),
            );
        }
        let outputs: Vec<_> = contenders
            .into_iter()
            .map(|child| child.wait_with_output().expect("wait concurrent status"))
            .collect();

        // A successful front door returns only after both broker pipes accept
        // a short probe. Give the detached broker log one final flush interval.
        let deadline = Instant::now() + Duration::from_secs(2);
        while count_substr(&broker_spawn_log(&root), "binding at") == 0 && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(500));
        let log = broker_spawn_log(&root);

        // Cleanup through the exact same path-derived control pipe before any
        // assertion can panic and leak the detached broker.
        stop_broker(&root, &program);
        stop_daemons_in_root(&root);

        assert!(
            outputs.iter().all(|output| output.status.success()),
            "every concurrent front door must observe the elected broker; outputs={:?}\n{log}",
            outputs
                .iter()
                .map(|output| (
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            count_substr(&log, "binding at"),
            1,
            "the concurrent cold start must elect exactly one broker\n{log}"
        );
        assert_eq!(
            count_substr(&log, "another broker already owns"),
            0,
            "SQLite election must prevent loser processes from spawning an \
             already-owned broker candidate\n{log}"
        );
    }
);

timed_test!(
    generated_cargo_shim_cold_starts_the_source_broker,
    Duration::from_secs(90),
    {
        let root = common::unique_temp_dir("cargo-shim-cold-root");
        let shim_dir = root.join("shims");

        let link_program = unique_program("link");
        let link_output = common::isolated_soldr_command()
            .args([
                "toolchain",
                "link",
                "--shim-dir",
                &shim_dir.display().to_string(),
            ])
            .env("SOLDR_CACHE_DIR", &root)
            .env("SOLDR_BROKER_PROGRAM", &link_program)
            .output()
            .expect("materialize toolchain shims");
        stop_broker(&root, &link_program);
        assert!(
            link_output.status.success(),
            "toolchain link failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&link_output.stdout),
            String::from_utf8_lossy(&link_output.stderr)
        );

        let program = unique_program("cargo");
        let cargo_shim = shim_dir.join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        let mut command = Command::new(&cargo_shim);
        common::scrub_outer_soldr_env(&mut command);
        let output = command
            .arg("--version")
            .env("SOLDR_CACHE_DIR", &root)
            .env("SOLDR_BROKER_PROGRAM", &program)
            .output()
            .expect("run cold cache-enabled generated cargo shim");

        let log = broker_spawn_log(&root);
        stop_broker(&root, &program);
        stop_daemons_in_root(&root);

        assert!(
            output.status.success(),
            "generated cargo shim failed its cold cache-enabled invocation\nstdout:\n{}\nstderr:\n{}\n{log}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("binding at") && line.contains(&program))
                .count(),
            1,
            "the generated cargo shim must cold-start exactly one source-identity broker\n{log}"
        );
    }
);
