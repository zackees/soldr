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
use wait_timeout::ChildExt;
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

fn wait_for_ready(cache_root: &Path, home_root: &Path, deadline: Instant) -> bool {
    // PID file is written before the accept loop binds the endpoint, so
    // it only proves the process started. The CLI contract this test
    // exercises is `daemon status`, so wait until that endpoint answers.
    let pid_file = cache_root
        .join("cache")
        .join("soldr-daemon")
        .join("daemon.pid");
    while Instant::now() < deadline {
        if pid_file.exists() && status_reports_running(cache_root, home_root) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn status_reports_running(cache_root: &Path, home_root: &Path) -> bool {
    let out = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|body| body["running"].as_bool())
        .unwrap_or(false)
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

fn run_soldr_with_timeout(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    timeout: Duration,
) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(args)
        .current_dir(current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    cmd.env("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "10000");
    cmd.env("SOLDR_COMPILE_REPLY_TIMEOUT_SECS", "60");
    cmd.env_remove("RUSTC_WRAPPER");

    let mut child = cmd.spawn().expect("failed to spawn soldr");
    if child
        .wait_timeout(timeout)
        .expect("failed waiting for soldr")
        .is_none()
    {
        let _ = child.kill();
        let output = child.wait_with_output().expect("collect timed-out output");
        panic!(
            "soldr {:?} timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            timeout,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    child.wait_with_output().expect("collect soldr output")
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
        // A cold embedded-zccache initialization can take ~25 seconds in
        // the shared Docker development runner. Keep the fixture bounded,
        // but do not misclassify that cold start as a multicall failure.
        let deadline = Instant::now() + Duration::from_secs(40);
        assert!(
            wait_for_ready(&cache_root, &home_root, deadline),
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

#[cfg(windows)]
struct DaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

#[cfg(windows)]
impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
    }
}

#[test]
fn start_status_stop_round_trip() {
    // `running_process_disable_uses_direct_daemon_liveness` mutates the
    // process-global RUNNING_PROCESS_DISABLE flag. Serialize the direct
    // `is_live` assertion with that test so parallel execution cannot switch
    // backend policy between the CLI status probe and this library probe.
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
fn direct_recovery_accepts_slim_via_self_daemon() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    let cache_root = unique_temp_dir("daemon-via-self-cache");
    let home_root = unique_temp_dir("daemon-via-self-home");
    let slim_bin_dir = unique_temp_dir("daemon-via-self-bin");
    let slim_soldr = slim_bin_dir.join(if cfg!(windows) { "soldr.exe" } else { "soldr" });
    fs::copy(common::soldr_bin(), &slim_soldr).expect("copy slim soldr executable");

    let run_slim = |args: &[&str]| {
        let mut cmd = Command::new(&slim_soldr);
        cmd.args(args);
        for (key, value) in isolated_env(&cache_root, &home_root) {
            cmd.env(key, value);
        }
        cmd.env("RUNNING_PROCESS_DISABLE", "1")
            .env_remove("RUSTC_WRAPPER");
        cmd.output().expect("run slim soldr")
    };

    let first = run_slim(&["daemon", "start", "--idle-timeout", "60"]);
    assert!(first.status.success(), "first slim start failed: {first:?}");
    assert!(
        wait_for_ready(
            &cache_root,
            &home_root,
            Instant::now() + Duration::from_secs(40)
        ),
        "slim via-self daemon did not become ready"
    );
    let paths = SoldrPaths::with_root(cache_root.clone());
    let first_pid = soldr_cli::daemon::lifecycle::is_live(&paths)
        .expect("direct liveness must accept a soldr-named daemon");

    let second = run_slim(&["daemon", "start", "--idle-timeout", "60"]);
    assert!(
        second.status.success(),
        "second slim start failed: {second:?}"
    );
    assert_eq!(
        soldr_cli::daemon::lifecycle::is_live(&paths),
        Some(first_pid),
        "recovery must preserve the already-live via-self daemon"
    );

    let stop = run_slim(&["daemon", "stop"]);
    assert!(stop.status.success(), "slim daemon stop failed: {stop:?}");
}

#[test]
fn doctor_uses_same_endpoint_as_daemon_status_for_cook_counts() {
    let daemon = Daemon::spawn();
    let cache_root = daemon.cache_root.clone();
    let home_root = daemon.home_root.clone();
    let workspace = unique_temp_dir("daemon-doctor-workspace");

    let status = run_soldr(&["daemon", "status", "--json"], &cache_root, &home_root);
    assert!(
        status.status.success(),
        "daemon status failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_body: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(status_body["running"].as_bool(), Some(true));

    let doctor = run_soldr_with_timeout(
        &["doctor", "--json"],
        &cache_root,
        &home_root,
        &workspace,
        Duration::from_secs(15),
    );
    assert!(
        doctor.status.success(),
        "doctor failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor_body: Value = serde_json::from_slice(&doctor.stdout).expect("doctor json");
    assert_eq!(
        doctor_body["cook"]["entries"].as_u64(),
        Some(0),
        "doctor must query the same live daemon endpoint as `soldr daemon status`: {doctor_body}"
    );
    assert_eq!(doctor_body["cook"]["total_bytes"].as_u64(), Some(0));
    assert_eq!(doctor_body["cook"]["hits_this_session"].as_u64(), Some(0));

    drop(daemon);
}

#[cfg(windows)]
#[test]
fn cargo_test_recovers_after_daemon_stop_without_herd_spawning() {
    let cache_root = unique_temp_dir("daemon-restart-cache");
    let home_root = unique_temp_dir("daemon-restart-home");
    let project = unique_temp_dir("daemon-restart-project");
    let _cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"soldr_daemon_restart_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::create_dir_all(project.join("src")).expect("create src");
    fs::write(
        project.join("src").join("lib.rs"),
        "pub fn add(left: usize, right: usize) -> usize { left + right }\n\
         #[test]\n\
         fn it_adds() { assert_eq!(add(2, 2), 4); }\n",
    )
    .expect("write lib.rs");

    let first = run_soldr_with_timeout(
        &["cargo", "test", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        Duration::from_secs(90),
    );
    assert!(
        first.status.success(),
        "first soldr cargo test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let stop = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    assert!(
        stop.status.success(),
        "daemon stop failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    std::thread::sleep(Duration::from_millis(500));
    fs::remove_dir_all(project.join("target")).expect("remove target for forced recompile");

    let second = run_soldr_with_timeout(
        &["cargo", "test", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        Duration::from_secs(90),
    );
    assert!(
        second.status.success(),
        "second soldr cargo test after daemon stop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    let lifecycle = fs::read_to_string(
        cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("lifecycle.jsonl"),
    )
    .unwrap_or_default();
    let spawn_count = lifecycle
        .lines()
        .filter(|line| line.contains("\"event\":\"spawn\""))
        .count();
    assert!(
        spawn_count <= 2,
        "two cargo test runs with one explicit stop should spawn at most one daemon each; lifecycle={lifecycle}"
    );
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
    // #1501 moved servicedef to the running-process v2 surface; the
    // remaining deferred item is the upstream-gated broker-owned
    // UpgradeDaemon handoff (see SOLDR_DAEMON_SERVICE_DEF_DEFERRED).
    assert!(body["deferred"]
        .as_array()
        .expect("deferred array")
        .iter()
        .any(|item| item
            .as_str()
            .is_some_and(|value| value.contains("UpgradeDaemon"))));

    // #1501: servicedefs are written as `.servicedef.v2` protobufs and
    // load through the protocol_v2 loader.
    let loaded = running_process::broker::protocol_v2::ServiceDefinitionLoader::new(&service_root)
        .load("soldr-daemon")
        .expect("running-process loader validates soldr servicedef");
    assert_eq!(loaded.service_name, "soldr-daemon");
    assert_eq!(
        loaded.isolation,
        running_process::broker::protocol_v2::BrokerIsolation::SharedBroker as i32,
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
