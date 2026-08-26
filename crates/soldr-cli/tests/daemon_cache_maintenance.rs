//! Docker/Linux acceptance coverage for the cache-incident ownership contract
//! (#1762–#1764). The same fixture is portable to the platform CI lanes.

#![allow(clippy::print_stdout)]

mod common;

use serde_json::Value;
use soldr_cli::core::SoldrPaths;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn daemon_bin() -> PathBuf {
    common::soldr_bin().parent().unwrap().join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        },
    )
}

fn command_env(command: &mut Command, root: &Path, home: &Path) {
    common::isolated_daemon::configure_isolated_daemon_client(command, &daemon_bin(), root);
    command
        .env("SOLDR_CACHE_DIR", root)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SOLDR_TEST_DIRECT_DAEMON_CONTROL", "1")
        .env_remove("RUSTC_WRAPPER");
}

fn run_soldr(root: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(common::soldr_bin());
    command.args(args);
    command_env(&mut command, root, home);
    command.output().expect("run soldr")
}

fn spawn_daemon(root: &Path, home: &Path) -> Child {
    let mut command = common::isolated_daemon::isolated_daemon_command(&daemon_bin(), root);
    command
        .args(["--foreground", "--idle-timeout-secs", "120"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command_env(&mut command, root, home);
    command.spawn().expect("spawn daemon")
}

fn wait_ready(root: &Path, home: &Path, deadline: Instant) -> Value {
    loop {
        let output = run_soldr(root, home, &["daemon", "status", "--json"]);
        if output.status.success() {
            if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
                if value["running"].as_bool() == Some(true) {
                    return value;
                }
            }
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_maintenance(root: &Path, deadline: Instant) -> Value {
    let paths = SoldrPaths::with_root(root.to_path_buf());
    let path = soldr_cli::daemon::maintenance::status_path(&paths);
    loop {
        if let Ok(body) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str(&body) {
                return value;
            }
        }
        assert!(
            Instant::now() < deadline,
            "maintenance status never appeared at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_daemon(root: &Path, home: &Path, child: &mut Child) {
    let _ = run_soldr(root, home, &["daemon", "stop"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn prod_dev_daemons_and_manual_orphan_maintenance_are_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let prod = temp.path().join(".soldr");
    let dev = temp.path().join(".soldr-dev");
    let custom = temp.path().join("custom");
    let standalone = temp.path().join(".zccache");
    for root in [&home, &prod, &dev, &custom, &standalone] {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("sentinel"), root.display().to_string()).unwrap();
    }

    // Spawn before waiting so cold embedded-service initialization happens
    // concurrently and proves the two broker/endpoint namespaces coexist.
    let mut prod_child = spawn_daemon(&prod, &home);
    let mut dev_child = spawn_daemon(&dev, &home);
    // soldr#2883: one deadline covers all four waits below, which is the right
    // model -- the two daemons were spawned above and initialize concurrently,
    // so what is bounded is the whole phase's wall clock, not each poll.
    //
    // What was wrong is its size: 60s did not cover two cold embedded-service
    // initializations on a contended runner. The darwin x64 lane exhausted it
    // during startup and failed at 62.8s with the other 2840 tests passing.
    //
    // 120s for the same reason the route budget is not measurement-plus-epsilon:
    // a bound reached by real work is the failure, so clearing the observation
    // by a hair just moves the cliff. It still sits under the 180s nextest
    // grants this test, so an actual wedge fails with this fixture's own
    // "daemon did not become ready" rather than a generic kill.
    let deadline = Instant::now() + Duration::from_secs(120);
    let prod_status = wait_ready(&prod, &home, deadline);
    let dev_status = wait_ready(&dev, &home, deadline);
    assert_ne!(prod_status["pid"], dev_status["pid"]);

    let prod_maintenance = wait_maintenance(&prod, deadline);
    let dev_maintenance = wait_maintenance(&dev, deadline);
    assert_eq!(
        PathBuf::from(prod_maintenance["owning_root"].as_str().unwrap()),
        prod
    );
    assert_eq!(
        PathBuf::from(dev_maintenance["owning_root"].as_str().unwrap()),
        dev
    );
    assert_ne!(
        prod_maintenance["embedded_cache_root"],
        dev_maintenance["embedded_cache_root"]
    );
    assert!(custom.join("sentinel").is_file());
    assert!(standalone.join("sentinel").is_file());

    let live_root_arg = prod.to_str().unwrap();
    let refused = run_soldr(
        &custom,
        &home,
        &["gc", "maintain", "--root", live_root_arg, "--json"],
    );
    assert!(
        !refused.status.success(),
        "manual maintenance must not race a live root owner"
    );

    stop_daemon(&prod, &home, &mut prod_child);
    stop_daemon(&dev, &home, &mut dev_child);

    let custom_paths = soldr_cli::core::SoldrPaths::with_root(custom.clone());
    let active_build =
        soldr_cli::cache_lib::build_active::BuildActivityLease::acquire(&custom_paths, 0x1762)
            .unwrap();
    let custom_arg = custom.to_str().unwrap();
    let deferred = run_soldr(
        &prod,
        &home,
        &["gc", "maintain", "--root", custom_arg, "--json"],
    );
    assert!(
        !deferred.status.success(),
        "a deferred manual pass must return a nonzero status"
    );
    let deferred_status: Value = serde_json::from_slice(&deferred.stdout).unwrap();
    assert_eq!(deferred_status["deferred_reason"], "build_active");
    drop(active_build);

    // Manual maintenance requires the exact orphan root and mutates only
    // that root. No home/sibling discovery is involved.
    let trash = custom.join("trash-X/old");
    std::fs::create_dir_all(&trash).unwrap();
    std::fs::write(trash.join("payload"), b"delete").unwrap();
    let output = run_soldr(
        &prod,
        &home,
        &["gc", "maintain", "--root", custom_arg, "--json"],
    );
    assert!(
        output.status.success(),
        "manual maintenance failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manual: Value = serde_json::from_slice(&output.stdout).unwrap();
    let reported_root = PathBuf::from(manual["owning_root"].as_str().unwrap());
    assert_eq!(
        std::fs::canonicalize(&reported_root).unwrap(),
        std::fs::canonicalize(&custom).unwrap(),
    );
    assert!(!trash.exists());
    assert!(prod.join("sentinel").is_file());
    assert!(dev.join("sentinel").is_file());
    assert!(standalone.join("sentinel").is_file());

    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let external = temp.path().join("external");
        let linked = temp.path().join("linked-root");
        std::fs::create_dir_all(external.join("trash-X/old")).unwrap();
        std::fs::write(external.join("trash-X/old/sentinel"), b"keep").unwrap();
        soldr_platform::fs::links::create(
            external.to_str().expect("UTF-8 external path"),
            &linked,
            true,
        )
        .unwrap();
        let linked_arg = linked.to_str().unwrap();
        let refused = run_soldr(
            &prod,
            &home,
            &["gc", "maintain", "--root", linked_arg, "--json"],
        );
        assert!(!refused.status.success());
        assert!(external.join("trash-X/old/sentinel").is_file());
    }
}

#[test]
fn delegated_pep517_pipe_lease_defers_maintenance_and_releases_on_eof() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("delegated-root");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(common::soldr_bin())
        .args(["gc", "hold-build-lease"])
        .env("SOLDR_CACHE_DIR", &root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let paths = soldr_cli::core::SoldrPaths::with_root(root);
    assert!(
        soldr_cli::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
            .unwrap()
            .is_none()
    );

    // Python crashing closes its write end of this pipe. Dropping it here
    // exercises the same EOF path without any graceful release message.
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    soldr_cli::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
        .unwrap()
        .expect("EOF releases delegated PEP517 build lease");
}
