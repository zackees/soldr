//! Multi-process SESSION production-path smoke (soldr#2388 Step 8 / #2361
//! Phase 3): a **real** `soldr broker serve` process plus a **real** soldr
//! RUSTC_WRAPPER invocation compile a tiny crate end-to-end through the broker
//! relay to the daemon. The broker + SESSION path is unconditional (soldr#2388),
//! so this exercises the default compile topology, not an opt-in.
//!
//! The in-process anchor (`session_real_compile_e2e`) proves the data path; this
//! proves the piece it cannot: the production **spawn** path — the broker as a
//! separate process, the daemon launched under it, and the client dialing the
//! broker's companion SESSION socket by program namespace. The
//! `SOLDR_SESSION_DEBUG` "SESSION compile served" marker proves SESSION actually
//! carried the compile rather than silently falling back to the legacy path.
//!
//! Heavy + multi-process by nature (per the codebase, this true-branch is what
//! the Docker harness covers); it runs under a generous `timed_test!` budget and
//! is designed to fail loudly in CI if the production SESSION path regresses.

mod common;

use soldr_cli::timed_test;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn unique_program(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("soldr-session-mp-{label}-{:012x}", nanos & 0xFFFF_FFFF_FFFF)
}

/// rustc as the sibling of cargo's `CARGO` env — dep-free, the pinned toolchain.
fn sibling_rustc() -> PathBuf {
    let cargo = std::env::var_os("CARGO").expect("CARGO set by cargo test");
    Path::new(&cargo).with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX))
}

use std::sync::{Arc, Mutex};

/// Drain a child stream on a background thread into a shared line buffer, so the
/// pipe never blocks and the full broker log is available for diagnostics.
fn drain_lines<R: std::io::Read + Send + 'static>(reader: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    lines
}

/// Wait until a collected line contains `needle`, or the deadline passes.
fn wait_for(lines: &Arc<Mutex<Vec<String>>>, needle: &str, deadline: Instant) -> bool {
    loop {
        if lines.lock().unwrap().iter().any(|l| l.contains(needle)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Recursively find every `daemon.pid` under the isolated root and return the
/// pids. Robust to the exact pidfile derivation. Used both to assert the
/// single-daemon invariant (#2364: one build → exactly one daemon) and to reap
/// the daemon in cleanup.
fn find_daemon_pids(root: &Path) -> Vec<u32> {
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
                    if let Some(pid) = text
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        out.push(pid);
                    }
                }
            }
        }
    }
    let mut pids = Vec::new();
    walk(root, &mut pids);
    pids
}

/// Kill any daemon this test's broker/client launched, so none leaks past the
/// test (a leaked daemon holding the isolated root's sockets/alias would
/// cascade-fail sibling tests in a shared nextest run).
fn stop_daemon_in_root(root: &Path) {
    let pids = find_daemon_pids(root);
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

timed_test!(
    session_compile_over_broker_multiprocess,
    Duration::from_secs(60),
    {
        let rustc = sibling_rustc();
        if !rustc.is_file() {
            eprintln!("skip: no sibling rustc at {rustc:?}");
            return;
        }

        let root_a = common::unique_temp_dir("session-mp-root-a");
        let root_b = common::unique_temp_dir("session-mp-root-b");
        let program = unique_program("prog");
        let project_a = root_a.join("workspace");
        let project_b = root_b.join("workspace");
        for (project, value) in [(&project_a, 909), (&project_b, 910)] {
            std::fs::create_dir_all(project.join("src")).expect("create project src");
            std::fs::write(
                project.join("src/lib.rs"),
                format!("pub fn mp() -> u32 {{ {value} }}\n"),
            )
            .expect("write source");
        }

        let daemon = common::soldr_bin()
            .with_file_name(format!("soldr-daemon{}", std::env::consts::EXE_SUFFIX));
        let service_root = running_process::broker::protocol_v2::service_definition_dir_v2();
        let route_a =
            soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
                &service_root,
                &soldr_cli::core::SoldrPaths::with_root(root_a.clone()),
                &daemon,
            )
            .expect("install root-a service definition");
        let route_b =
            soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
                &service_root,
                &soldr_cli::core::SoldrPaths::with_root(root_b.clone()),
                &daemon,
            )
            .expect("install root-b service definition");
        assert_ne!(
            route_a.definition.service_name,
            route_b.definition.service_name
        );

        // 1) Spawn the real broker on the isolated cache root. It installs the
        //    servicedef, binds the control socket, and serves the companion
        //    SESSION relay to the daemon's deterministic endpoint.
        let mut broker = Command::new(common::soldr_bin())
            .args(["broker", "serve", "--program", &program])
            .env("SOLDR_CACHE_DIR", &root_a)
            .env("SOLDR_BROKER_DEBUG", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn soldr broker serve");

        let broker_out = drain_lines(broker.stdout.take().expect("broker stdout"));
        let broker_err = drain_lines(broker.stderr.take().expect("broker stderr"));
        // Wait for the SESSION relay to actually BIND (not just the control
        // socket) so the client never races the relay's bind.
        let bound = wait_for(
            &broker_out,
            "SESSION relay bound at",
            Instant::now() + Duration::from_secs(30),
        );

        // 2) Run soldr in RUSTC_WRAPPER mode (argv[1] = rustc) through the SESSION
        //    hot path: it ensures the daemon under the broker, then relays the
        //    compile client -> broker -> daemon.
        let spawn_compile = |root: &Path, project: &Path, crate_name: &str| {
            let mut cmd = Command::new(common::soldr_bin());
            common::scrub_outer_soldr_env(&mut cmd);
            cmd.arg(&rustc)
                .args([
                    "--edition",
                    "2021",
                    "--crate-type",
                    "lib",
                    "--crate-name",
                    crate_name,
                    "--emit=metadata",
                    "-C",
                    "metadata=mp1",
                    "--out-dir",
                    "target/debug/deps",
                    "src/lib.rs",
                ])
                .current_dir(project)
                .env("SOLDR_CACHE_DIR", root)
                .env("SOLDR_SESSION_DEBUG", "1")
                .env("SOLDR_BROKER_PROGRAM", &program)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn soldr RUSTC_WRAPPER over SESSION")
        };
        let compile_a = spawn_compile(&root_a, &project_a, "soldr_session_mp_a");
        let compile_b = spawn_compile(&root_b, &project_b, "soldr_session_mp_b");
        let out_a = compile_a.wait_with_output().expect("root-a compile");
        let out_b = compile_b.wait_with_output().expect("root-b compile");

        // #2364 singleton invariant: one build routed through the broker
        // produces exactly one daemon (no spawn-storm, no double-daemon).
        // Captured before cleanup; the pidfiles persist after the kill.
        let daemon_pids_a = find_daemon_pids(&root_a);
        let daemon_pids_b = find_daemon_pids(&root_b);

        // Cleanup before asserting so a failure never leaks processes.
        let _ = broker.kill();
        let _ = broker.wait();
        stop_daemon_in_root(&root_a);
        stop_daemon_in_root(&root_b);

        let blog = |label: &str, l: &Arc<Mutex<Vec<String>>>| {
            format!("{label}:\n  {}", l.lock().unwrap().join("\n  "))
        };
        let broker_log = format!(
            "{}\n{}",
            blog("broker stdout", &broker_out),
            blog("broker stderr", &broker_err)
        );
        let stdout_a = String::from_utf8_lossy(&out_a.stdout);
        let stderr_a = String::from_utf8_lossy(&out_a.stderr);
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        let stderr_b = String::from_utf8_lossy(&out_b.stderr);
        assert!(
            bound,
            "broker SESSION relay did not bind within 30s\n{broker_log}"
        );
        assert!(
            out_a.status.success() && out_b.status.success(),
            "SESSION wrapper compile failed\nroot-a stdout:\n{stdout_a}\nroot-a stderr:\n{stderr_a}\nroot-b stdout:\n{stdout_b}\nroot-b stderr:\n{stderr_b}\n{broker_log}"
        );
        assert!(
            stderr_a.contains("SESSION compile served")
                && stderr_b.contains("SESSION compile served"),
            "SESSION did not carry both compiles (silent legacy fallback?)\n\
             root-a stderr:\n{stderr_a}\nroot-b stderr:\n{stderr_b}\n{broker_log}"
        );
        assert_eq!(
            daemon_pids_a.len(),
            1,
            "root A must own one daemon; found {daemon_pids_a:?}\n{broker_log}"
        );
        assert_eq!(
            daemon_pids_b.len(),
            1,
            "root B must own one daemon; found {daemon_pids_b:?}\n{broker_log}"
        );
        assert_ne!(
            daemon_pids_a, daemon_pids_b,
            "distinct roots must never share daemon ownership"
        );
    }
);
