//! Issue #1286 (F1): `soldr cache flush` must checkpoint the
//! soldr-daemon's EMBEDDED zccache state (artifact index, depgraph
//! snapshot, metadata cache) to disk via `Request::FlushCaches`.
//!
//! Before the fix, `cache flush` / `cache shutdown` did not checkpoint
//! the embedded rustc-side state. It stayed memory-only until a graceful
//! daemon exit, so `soldr save` archives taken from a live daemon restored
//! with zero rustc hits (the cold-tar-untar-warm 1.00x-speedup bug).

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;

/// How long to allow between a synchronous stop returning and the child
/// becoming reapable via `try_wait` (soldr#1891).
///
/// The property under test is that `daemon stop` / `cache shutdown` are
/// **synchronous** — they must not return while the daemon is still running.
/// Asserting `try_wait().is_some()` with zero tolerance also asserts that the
/// OS has already made the exit visible to the parent handle, which is not
/// part of the contract and which fails under a loaded parallel test run.
///
/// This margin cannot mask an actually-asynchronous stop: these fixtures spawn
/// the daemon with `--idle-timeout-secs 60`, so a stop that returned without
/// the daemon exiting would leave it alive ~30x beyond this window and still
/// fail the assertion.
const EXIT_VISIBLE_TOLERANCE: Duration = Duration::from_secs(2);

/// Wait up to [`EXIT_VISIBLE_TOLERANCE`] for `child` to become reapable.
///
/// Returns true if it exited within the window.
fn exited_within_tolerance(child: &mut Child) -> bool {
    let deadline = Instant::now() + EXIT_VISIBLE_TOLERANCE;
    loop {
        match child.try_wait().expect("query daemon child") {
            Some(_) => return true,
            None if Instant::now() >= deadline => return false,
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn soldr_daemon_bin() -> PathBuf {
    let soldr = common::soldr_bin();
    let parent = soldr.parent().expect("parent");
    let stem = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    common::isolated_daemon::configure_isolated_daemon_client(
        &mut cmd,
        &soldr_daemon_bin(),
        cache_root,
    );
    cmd.args(args)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env("SOLDR_TEST_DIRECT_DAEMON_CONTROL", "1")
        .env_remove("RUSTC_WRAPPER");
    cmd.output().expect("run soldr")
}

struct DaemonProc {
    child: Option<Child>,
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd =
            common::isolated_daemon::isolated_daemon_command(&soldr_daemon_bin(), cache_root);
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(90);
        let pid_file = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let mut ready = false;
        while Instant::now() < deadline {
            if pid_file.exists() {
                let status = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
                if status.status.success()
                    && serde_json::from_slice::<serde_json::Value>(&status.stdout)
                        .ok()
                        .and_then(|body| body["running"].as_bool())
                        .unwrap_or(false)
                {
                    ready = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "isolated daemon never became ready under {}",
                cache_root.display()
            );
        }
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
            home_root: home_root.to_path_buf(),
        }
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

#[test]
fn cache_flush_checkpoints_embedded_state() {
    let cache_root = unique_temp_dir("flush-caches-cache");
    let home_root = unique_temp_dir("flush-caches-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);

    let out = run_soldr(&["cache", "flush", "--json"], &cache_root, &home_root);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "cache flush must exit 0; stdout: {stdout}; stderr: {stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("cache flush must emit valid JSON");
    assert_eq!(report["flushed"], true, "flush report: {report}");
    assert_eq!(report["stats"]["complete"], true, "flush report: {report}");
    assert_eq!(
        report["stats"]["pending_writes_drained"], true,
        "flush report: {report}"
    );
    assert_eq!(
        report["stats"]["index_writer_drained"], true,
        "flush report: {report}"
    );

    let steps = report["stats"]["steps"]
        .as_array()
        .expect("flush report must contain step outcomes");
    assert!(
        steps
            .iter()
            .all(|step| { step["status"] == "completed" && step["error"].is_null() }),
        "every checkpoint step must complete successfully: {report}"
    );
    let mut step_names: Vec<_> = steps
        .iter()
        .map(|step| step["step"].as_str().expect("flush step must have a name"))
        .collect();
    step_names.sort_unstable();
    assert_eq!(
        step_names,
        [
            "artifact_store",
            "compiler_hash",
            "depgraph",
            "metadata",
            "system_includes",
        ],
        "flush report must account for every embedded checkpoint: {report}"
    );

    // The checkpoint must leave the embedded depgraph snapshot
    // durable on disk — this is the file whose absence made
    // archives restore with zero rustc hits.
    let zccache_root = cache_root.join("cache").join("zccache");
    assert!(
        find_file(&zccache_root, "depgraph.bin").is_some(),
        "embedded depgraph snapshot must exist under {} after flush",
        zccache_root.display()
    );

    drop(daemon);
}

#[test]
fn cache_shutdown_stops_soldr_daemon_and_waits_for_exit() {
    let cache_root = unique_temp_dir("shutdown-cache");
    let home_root = unique_temp_dir("shutdown-home");
    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);

    let out = run_soldr(
        &[
            "cache",
            "shutdown",
            "--shutdown-timeout-seconds",
            "30",
            "--json",
        ],
        &cache_root,
        &home_root,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "cache shutdown must complete successfully; stdout: {stdout}; stderr: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("cache shutdown JSON");
    assert_eq!(json["daemon_was_running"], true);
    assert_eq!(json["shutdown_requested"], true);
    assert_eq!(json["daemon_exited"], true);
    assert_eq!(json["flush"]["complete"], true);

    let child = daemon.child.as_mut().expect("daemon child");
    assert!(
        exited_within_tolerance(child),
        "cache shutdown returned before the soldr daemon exited"
    );
    daemon.child = None;
}

#[test]
fn daemon_stop_does_not_return_before_process_exit() {
    let cache_root = unique_temp_dir("daemon-stop-cache");
    let home_root = unique_temp_dir("daemon-stop-home");
    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);

    let out = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    assert!(
        out.status.success(),
        "daemon stop failed; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let child = daemon.child.as_mut().expect("daemon child");
    assert!(
        exited_within_tolerance(child),
        "daemon stop returned before the process exited"
    );
    daemon.child = None;
}
