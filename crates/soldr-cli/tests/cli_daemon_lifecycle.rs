//! Integration coverage for `soldr daemon start --foreground` /
//! `soldr daemon status` / `soldr daemon stop`. Verifies the daemon
//! comes up, answers status, shuts down cleanly, and leaves no PID
//! or socket file behind.

#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use soldr_cli::core::SoldrPaths;
mod common;


static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvScope {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvScope {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        match &self.prior {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
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
    let parent = soldr.parent().expect("CARGO_BIN_EXE_soldr has a parent");
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn isolated_env(cache_root: &Path, home_root: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("SOLDR_CACHE_DIR", cache_root.as_os_str().to_os_string()),
        ("HOME", home_root.as_os_str().to_os_string()),
        ("USERPROFILE", home_root.as_os_str().to_os_string()),
    ]
}

fn wait_for_ready(cache_root: &Path, deadline: Instant) -> bool {
    // PID file is written before the accept loop binds the endpoint
    // and is the strongest cross-platform "the daemon process is up"
    // signal — Unix socket paths may relocate to $TMPDIR under
    // SUN_LEN constraints, and Windows named pipes aren't a fs entry.
    let pid_file = cache_root
        .join("cache")
        .join("soldr-daemon")
        .join("daemon.pid");
    while Instant::now() < deadline {
        if pid_file.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(args);
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    cmd.env_remove("RUSTC_WRAPPER");
    cmd.output().expect("failed to run soldr")
}

struct Daemon {
    child: Option<Child>,
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Daemon {
    fn spawn() -> Self {
        let cache_root = unique_temp_dir("daemon-lifecycle-cache");
        let home_root = unique_temp_dir("daemon-lifecycle-home");
        let mut cmd = Command::new(soldr_daemon_bin());
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in isolated_env(&cache_root, &home_root) {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(
            wait_for_ready(&cache_root, deadline),
            "daemon never opened its endpoint under {}",
            cache_root.display()
        );
        Self {
            child: Some(child),
            cache_root,
            home_root,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
            let deadline = Instant::now() + Duration::from_secs(2);
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

#[test]
fn start_status_stop_round_trip() {
    let daemon = Daemon::spawn();
    let cache_root = daemon.cache_root.clone();
    let home_root = daemon.home_root.clone();

    let status = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        status.status.success(),
        "soldr daemon status failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let body: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(body["running"].as_bool(), Some(true));
    let pid = body["pid"].as_u64().expect("status carries pid");
    assert!(pid > 0);
    let paths = SoldrPaths::with_root(cache_root.clone());
    assert_eq!(
        soldr_cli::daemon::lifecycle::is_live(&paths).map(u64::from),
        Some(pid),
        "lifecycle::is_live must verify the daemon through running-process BackendHandle",
    );

    let stop = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    assert!(stop.status.success(), "stop failed: {stop:?}");

    drop(daemon);

    // The PID file is the canonical "did the daemon leave anything
    // behind" signal; the socket path can relocate to $TMPDIR under
    // SUN_LEN so checking its absence is brittle.
    let pid_path = cache_root
        .join("cache")
        .join("soldr-daemon")
        .join("daemon.pid");
    assert!(
        !pid_path.exists(),
        "pid file left behind at {}",
        pid_path.display()
    );
}

#[test]
fn running_process_disable_uses_direct_daemon_liveness() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let daemon = Daemon::spawn();
    let cache_root = daemon.cache_root.clone();
    let home_root = daemon.home_root.clone();

    let status = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        status.status.success(),
        "soldr daemon status failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let body: Value = serde_json::from_slice(&status.stdout).expect("status json");
    let pid = body["pid"].as_u64().expect("status carries pid");

    let _env = EnvScope::set("RUNNING_PROCESS_DISABLE", "1");
    let paths = SoldrPaths::with_root(cache_root);
    assert_eq!(
        soldr_cli::daemon::lifecycle::is_live(&paths).map(u64::from),
        Some(pid),
        "RUNNING_PROCESS_DISABLE=1 should bypass BackendHandle but keep direct daemon liveness",
    );

    drop(daemon);
}

#[test]
fn status_when_daemon_absent_reports_not_running() {
    let cache_root = unique_temp_dir("daemon-absent-cache");
    let home_root = unique_temp_dir("daemon-absent-home");
    let out = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        out.status.success(),
        "status against absent daemon should succeed (exit=0). stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("status json");
    assert_eq!(body["running"].as_bool(), Some(false));
}

#[test]
fn install_servicedef_writes_running_process_definition() {
    let cache_root = unique_temp_dir("daemon-servicedef-cache");
    let home_root = unique_temp_dir("daemon-servicedef-home");
    let service_root = unique_temp_dir("daemon-servicedef-services");
    let daemon_dir = unique_temp_dir("daemon-servicedef-bin");
    let daemon_binary = daemon_dir.join(if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    });
    fs::write(&daemon_binary, b"stub daemon").expect("write fake daemon binary");

    let mut cmd = Command::new(common::soldr_bin());
    cmd.args([
        "daemon",
        "install-servicedef",
        "--daemon-binary",
        daemon_binary.to_str().expect("utf8 daemon path"),
        "--json",
    ]);
    for (k, v) in isolated_env(&cache_root, &home_root) {
        cmd.env(k, v);
    }
    cmd.env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root);
    cmd.env_remove("RUSTC_WRAPPER");
    let out = cmd.output().expect("run soldr daemon install-servicedef");

    assert!(
        out.status.success(),
        "install-servicedef failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body: Value = serde_json::from_slice(&out.stdout).expect("servicedef json");
    assert_eq!(body["service_name"].as_str(), Some("soldr-daemon"));
    assert!(body["deferred"]
        .as_array()
        .expect("deferred array")
        .iter()
        .any(|item| item
            .as_str()
            .is_some_and(|value| value.contains("connect_to_backend"))));

    let loaded = running_process::broker::server::ServiceDefinitionLoader::new(&service_root)
        .load("soldr-daemon")
        .expect("running-process loader validates soldr servicedef");
    assert_eq!(loaded.service_name, "soldr-daemon");
    assert_eq!(
        loaded.isolation,
        running_process::broker::protocol::BrokerIsolation::SharedBroker as i32,
    );
    assert_eq!(
        loaded.binary_path,
        fs::canonicalize(&daemon_binary)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(loaded.min_version, env!("CARGO_PKG_VERSION"));
}
