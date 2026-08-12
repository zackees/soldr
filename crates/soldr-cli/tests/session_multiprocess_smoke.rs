//! Multi-process SESSION production-path smoke (soldr#2388 Step 8 / #2361
//! Phase 3): a **real** `soldr broker serve` process plus a **real** soldr
//! RUSTC_WRAPPER invocation compile a tiny crate end-to-end through the broker
//! relay to the daemon. The broker + SESSION path is unconditional (soldr#2388),
//! so this exercises the default compile topology, not an opt-in.
//!
//! This proves the production **spawn** path — the broker as a separate process,
//! the daemon launched under it, and the client dialing the broker's stable endpoint. The
//! `SOLDR_SESSION_DEBUG` "SESSION compile served" marker proves SESSION actually
//! carried the compile.
//!
//! Heavy + multi-process by nature (per the codebase, this true-branch is what
//! the Docker harness covers); it runs under a generous `timed_test!` budget and
//! is designed to fail loudly in CI if the production SESSION path regresses.

mod common;

use soldr_cli::timed_test;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Read the isolated root's protobuf route claim. Used both to assert the
/// single-daemon invariant and to reap the daemon in cleanup.
fn find_daemon_pids(root: &Path) -> Vec<u32> {
    soldr_cli::daemon::backend_handle_adoption::read_broker_route_claim(
        &soldr_cli::core::SoldrPaths::with_root(root.to_path_buf()),
    )
    .ok()
    .flatten()
    .map(|claim| vec![claim.pid])
    .unwrap_or_default()
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
        let home = common::unique_temp_dir("session-mp-home");
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
        let service_root = home.join("service-definitions");
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

        // 1) Spawn the real broker for the isolated home. The same stable
        //    endpoint serves admin, Hello negotiation, and SESSION bytes.
        let mut broker = Command::new(common::soldr_bin())
            .args(["broker", "serve"])
            .env("SOLDR_CACHE_DIR", &root_a)
            .env("SOLDR_BROKER_DEBUG", "1")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn soldr broker serve");

        let broker_out = drain_lines(broker.stdout.take().expect("broker stdout"));
        let broker_err = drain_lines(broker.stderr.take().expect("broker stderr"));
        // Wait for the one admin+Hello+SESSION endpoint to bind so clients do
        // not race readiness.
        let bound = wait_for(
            &broker_out,
            "stable endpoint bound at",
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
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn soldr RUSTC_WRAPPER over SESSION")
        };
        let compile_a = spawn_compile(&root_a, &project_a, "soldr_session_mp_a");
        let compile_a2 = spawn_compile(&root_a, &project_a, "soldr_session_mp_a2");
        let compile_b = spawn_compile(&root_b, &project_b, "soldr_session_mp_b");
        let out_a = compile_a.wait_with_output().expect("root-a compile");
        let out_a2 = compile_a2
            .wait_with_output()
            .expect("root-a second compile");
        let out_b = compile_b.wait_with_output().expect("root-b compile");

        // #2364 singleton invariant: one build routed through the broker
        // produces exactly one daemon (no spawn-storm, no double-daemon).
        // Captured before cleanup; protobuf claims persist after the kill.
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
        let stdout_a2 = String::from_utf8_lossy(&out_a2.stdout);
        let stderr_a2 = String::from_utf8_lossy(&out_a2.stderr);
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        let stderr_b = String::from_utf8_lossy(&out_b.stderr);
        assert!(
            bound,
            "stable broker endpoint did not bind within 30s\n{broker_log}"
        );
        assert!(
            out_a.status.success() && out_a2.status.success() && out_b.status.success(),
            "SESSION wrapper compile failed\nroot-a stdout:\n{stdout_a}\nroot-a stderr:\n{stderr_a}\nroot-a2 stdout:\n{stdout_a2}\nroot-a2 stderr:\n{stderr_a2}\nroot-b stdout:\n{stdout_b}\nroot-b stderr:\n{stderr_b}\n{broker_log}"
        );
        assert!(
            stderr_a.contains("SESSION compile served")
                && stderr_a2.contains("SESSION compile served")
                && stderr_b.contains("SESSION compile served"),
            "SESSION did not carry all concurrent compiles\n\
             root-a stderr:\n{stderr_a}\nroot-a2 stderr:\n{stderr_a2}\nroot-b stderr:\n{stderr_b}\n{broker_log}"
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

timed_test!(
    issue_2476_handed_off_compile_survives_broker_death_and_daemon_is_readopted,
    Duration::from_secs(120),
    {
        let rustc = sibling_rustc();
        if !rustc.is_file() {
            eprintln!("skip: no sibling rustc at {rustc:?}");
            return;
        }

        let root = common::unique_temp_dir("session-handoff-survival-root");
        let home = common::unique_temp_dir("session-handoff-survival-home");
        let service_root = home.join("service-definitions");
        let project = root.join("workspace");
        let session_ready = root.join("session-started");
        std::fs::create_dir_all(project.join("src")).expect("create project src");
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn survives() -> u32 { 2476 }\n",
        )
        .expect("write source");

        let daemon = common::soldr_daemon_bin();
        soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
            &service_root,
            &soldr_cli::core::SoldrPaths::with_root(root.clone()),
            &daemon,
        )
        .expect("install service definition");

        let spawn_broker = |disable_handoff: bool| {
            let mut command = Command::new(common::soldr_bin());
            command
                .args(["broker", "serve"])
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if disable_handoff {
                command.env("SOLDR_TEST_BROKER_DISABLE_HANDOFF", "1");
            }
            let mut broker = command.spawn().expect("spawn broker");
            let stdout = drain_lines(broker.stdout.take().expect("broker stdout"));
            let stderr = drain_lines(broker.stderr.take().expect("broker stderr"));
            assert!(
                wait_for(
                    &stdout,
                    "stable endpoint bound at",
                    Instant::now() + Duration::from_secs(30)
                ),
                "broker did not bind\nstdout={:?}\nstderr={:?}",
                stdout.lock().unwrap(),
                stderr.lock().unwrap()
            );
            (broker, stdout, stderr)
        };
        let spawn_compile = |pause: bool| {
            let mut command = Command::new(common::soldr_bin());
            common::scrub_outer_soldr_env(&mut command);
            command
                .arg(&rustc)
                .args([
                    "--edition",
                    "2021",
                    "--crate-type",
                    "lib",
                    "--crate-name",
                    "soldr_handoff_survival",
                    "--emit=metadata",
                    "-C",
                    "metadata=handoff-survival",
                    "--out-dir",
                    "target/debug/deps",
                    "src/lib.rs",
                ])
                .current_dir(&project)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env("SOLDR_CACHE_DIR", &root)
                .env("SOLDR_SESSION_DEBUG", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if pause {
                command
                    .env("SOLDR_TEST_SESSION_COMPILE_PAUSE_MS", "3000")
                    .env("SOLDR_TEST_SESSION_COMPILE_READY_FILE", &session_ready);
            }
            command.spawn().expect("spawn SESSION compile")
        };

        let (mut first_broker, first_stdout, first_stderr) = spawn_broker(false);
        let first_compile = spawn_compile(true);
        let ready_deadline = Instant::now() + Duration::from_secs(40);
        while !session_ready.exists() && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            session_ready.exists(),
            "daemon never received SessionStart\nstdout={:?}\nstderr={:?}",
            first_stdout.lock().unwrap(),
            first_stderr.lock().unwrap()
        );

        // A successful handoff has already removed the broker from this data
        // path. Killing it must not cut the paused compile.
        first_broker.kill().expect("kill first broker");
        let _ = first_broker.wait();
        let first_output = first_compile.wait_with_output().expect("first compile");
        let before = find_daemon_pids(&root);

        let (mut second_broker, second_stdout, second_stderr) = spawn_broker(false);
        let second_output = spawn_compile(false)
            .wait_with_output()
            .expect("compile through replacement broker");
        let after = find_daemon_pids(&root);

        let _ = second_broker.kill();
        let _ = second_broker.wait();

        // Force the portable same-connection proxy fallback. Unlike the
        // handed-off compile above, this SESSION is owned by the broker and
        // must fail promptly and explicitly when that broker dies.
        let _ = std::fs::remove_file(&session_ready);
        let (mut proxy_broker, proxy_stdout, proxy_stderr) = spawn_broker(true);
        let proxy_compile = spawn_compile(true);
        let proxy_ready_deadline = Instant::now() + Duration::from_secs(30);
        while !session_ready.exists() && Instant::now() < proxy_ready_deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            session_ready.exists(),
            "proxy fallback never delivered SessionStart\nstdout={:?}\nstderr={:?}",
            proxy_stdout.lock().unwrap(),
            proxy_stderr.lock().unwrap()
        );
        let proxy_cut_started = Instant::now();
        proxy_broker.kill().expect("kill proxy broker");
        let _ = proxy_broker.wait();
        let proxy_output = proxy_compile.wait_with_output().expect("proxy compile");
        let proxy_cut_elapsed = proxy_cut_started.elapsed();
        stop_daemon_in_root(&root);

        assert!(
            first_output.status.success(),
            "handed-off compile died with broker\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&first_output.stdout),
            String::from_utf8_lossy(&first_output.stderr)
        );
        assert!(
            second_output.status.success(),
            "replacement broker compile failed\nstdout:\n{}\nstderr:\n{}\nbroker stdout={:?}\nbroker stderr={:?}",
            String::from_utf8_lossy(&second_output.stdout),
            String::from_utf8_lossy(&second_output.stderr),
            second_stdout.lock().unwrap(),
            second_stderr.lock().unwrap()
        );
        assert_eq!(before.len(), 1, "first broker must launch one daemon");
        assert_eq!(
            after, before,
            "replacement broker must re-adopt the exact live daemon claim"
        );
        assert!(
            !proxy_output.status.success(),
            "proxy-fallback compile unexpectedly survived broker death"
        );
        assert!(
            proxy_cut_elapsed < Duration::from_secs(5),
            "proxy-fallback broker death was not prompt: {proxy_cut_elapsed:?}"
        );
        assert!(
            String::from_utf8_lossy(&proxy_output.stderr).contains("SESSION"),
            "proxy-fallback failure was not attributed to SESSION: {}",
            String::from_utf8_lossy(&proxy_output.stderr)
        );
    }
);
