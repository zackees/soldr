//! Integration coverage for `soldr daemon start --foreground` /
//! `soldr daemon status` / `soldr daemon stop`. Verifies the daemon
//! comes up, answers status, and shuts down cleanly. Retired daemons
//! deliberately leave stale endpoint claims for the next root owner
//! to reclaim.

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

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole};
#[cfg(windows)]
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

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

fn scrub_outer_soldr_runtime(cmd: &mut Command) {
    cmd.env_remove("RUSTC_WRAPPER")
        // A dogfooded outer `soldr cargo test` exports its installed daemon
        // image for compiler-child recovery. Fixtures must exercise the
        // binaries built for this test invocation instead.
        .env_remove(soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR)
        .env_remove("SOLDR_ORIGINAL_EXE")
        .env_remove("SOLDR_RELOCATED_EXE");
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
    scrub_outer_soldr_runtime(&mut cmd);
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
    scrub_outer_soldr_runtime(&mut cmd);

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
        scrub_outer_soldr_runtime(&mut cmd);
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

struct DaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    exe: String,
}

#[cfg(windows)]
fn process_snapshot() -> Vec<ProcessEntry> {
    // SAFETY: the snapshot handle is checked, PROCESSENTRY32W has the
    // required size, and the handle is closed on every successful path.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        assert_ne!(snapshot, INVALID_HANDLE_VALUE, "snapshot Windows processes");
        let mut raw: PROCESSENTRY32W = std::mem::zeroed();
        raw.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut entries = Vec::new();
        if Process32FirstW(snapshot, &mut raw) != 0 {
            loop {
                let length = raw
                    .szExeFile
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(raw.szExeFile.len());
                entries.push(ProcessEntry {
                    pid: raw.th32ProcessID,
                    parent_pid: raw.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&raw.szExeFile[..length]),
                });
                if Process32NextW(snapshot, &mut raw) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        entries
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: the process handle is opened for a read-only query and closed
    // before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let queried = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        queried != 0 && exit_code == STILL_ACTIVE as u32
    }
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !process_is_alive(pid)
}

#[cfg(windows)]
struct DetachedDaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

#[cfg(windows)]
impl Drop for DetachedDaemonCleanup {
    fn drop(&mut self) {
        let pid = soldr_cli::daemon::lifecycle::read_pid_file(&SoldrPaths::with_root(
            self.cache_root.clone(),
        ))
        .map(|(pid, _)| pid);
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
        if let Some(pid) = pid {
            let _ = wait_for_process_exit(pid, Duration::from_secs(5));
        }
        let _ = fs::remove_dir_all(&self.cache_root);
        let _ = fs::remove_dir_all(&self.home_root);
    }
}

#[cfg(windows)]
#[test]
#[ignore = "invoked by managed_windows_start_has_one_consoleless_owner"]
fn windows_daemon_console_probe_helper() {
    let pid: u32 = std::env::var("SOLDR_CONSOLE_PROBE_PID")
        .expect("console probe PID")
        .parse()
        .expect("numeric console probe PID");
    // SAFETY: this helper is an isolated test process. Detaching its inherited
    // console cannot affect the parent test runner; AttachConsole is a
    // read-only probe of whether the daemon owns a console.
    unsafe {
        let _ = FreeConsole();
        let attached = AttachConsole(pid);
        if attached != 0 {
            let _ = FreeConsole();
            panic!("daemon PID {pid} owns a Windows console");
        }
    }
}

#[cfg(windows)]
soldr_cli::timed_test!(
    managed_windows_start_has_one_consoleless_owner,
    Duration::from_secs(120),
    {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let cache_root = unique_temp_dir("daemon-detached-process-tree-cache");
        let home_root = unique_temp_dir("daemon-detached-process-tree-home");
        let _cleanup = DetachedDaemonCleanup {
            cache_root: cache_root.clone(),
            home_root: home_root.clone(),
        };

        let first = run_soldr(
            &["daemon", "start", "--idle-timeout", "60"],
            &cache_root,
            &home_root,
        );
        assert!(
            first.status.success(),
            "first detached start failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&first.stdout),
            String::from_utf8_lossy(&first.stderr),
        );
        assert!(
            wait_for_ready(
                &cache_root,
                &home_root,
                Instant::now() + Duration::from_secs(40)
            ),
            "managed daemon never became ready"
        );

        let paths = SoldrPaths::with_root(cache_root.clone());
        let (pid, exe) =
            soldr_cli::daemon::lifecycle::read_pid_file(&paths).expect("daemon PID publication");
        assert!(
            exe.starts_with(soldr_cli::self_relocate::daemon_runtime_root(&paths)),
            "the PID owner must be the canonical runtime image: {}",
            exe.display()
        );
        assert!(
            soldr_cli::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
                .expect("probe root owner lock")
                .is_none(),
            "the live PID owner must hold the root lock"
        );

        let second = run_soldr(
            &["daemon", "start", "--idle-timeout", "60"],
            &cache_root,
            &home_root,
        );
        assert!(
            second.status.success(),
            "second detached start failed: {second:?}"
        );
        assert_eq!(
            soldr_cli::daemon::lifecycle::read_pid_file(&paths).map(|(pid, _)| pid),
            Some(pid),
            "a second managed start must preserve the one root owner"
        );

        let processes = process_snapshot();
        let daemon = processes
            .iter()
            .find(|entry| entry.pid == pid)
            .unwrap_or_else(|| panic!("daemon PID {pid} missing from process snapshot"));
        assert_eq!(
            daemon.exe.to_ascii_lowercase(),
            "soldr-daemon.exe",
            "PID file must identify the canonical daemon process"
        );
        assert!(
            processes.iter().all(|entry| entry.pid != daemon.parent_pid),
            "the relocation trampoline PID {} is still alive: {processes:#?}",
            daemon.parent_pid
        );
        assert!(
            processes.iter().all(|entry| {
                entry.parent_pid != pid
                    || !matches!(
                        entry.exe.to_ascii_lowercase().as_str(),
                        "soldr-daemon.exe" | "conhost.exe"
                    )
            }),
            "daemon PID {pid} owns a duplicate daemon or conhost: {processes:#?}"
        );

        let probe = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--ignored",
                "--exact",
                "windows_daemon_console_probe_helper",
                "--nocapture",
            ])
            .env("SOLDR_CONSOLE_PROBE_PID", pid.to_string())
            .output()
            .expect("spawn isolated console probe");
        assert!(
            probe.status.success(),
            "daemon console probe failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr),
        );

        let stop = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
        assert!(
            stop.status.success(),
            "daemon stop failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&stop.stdout),
            String::from_utf8_lossy(&stop.stderr),
        );
        assert!(
            wait_for_process_exit(pid, Duration::from_secs(5)),
            "daemon PID {pid} survived stop"
        );
        assert!(
            soldr_cli::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
                .expect("probe released root owner lock")
                .is_some(),
            "daemon stop must release the root lock"
        );
    }
);

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

    // Shared claims remain as stale evidence. Startup reclaims them while it
    // owns the root; retirement never performs a racy check-then-unlink.
    assert_eq!(
        soldr_cli::daemon::lifecycle::read_pid_file(&paths).map(|(pid, _)| u64::from(pid)),
        Some(pid),
        "retirement should preserve the stopped generation's PID claim"
    );
    assert_eq!(
        soldr_cli::daemon::lifecycle::is_live(&paths),
        None,
        "a retained PID claim must not count as a live daemon"
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
        cmd.env("RUNNING_PROCESS_DISABLE", "1");
        scrub_outer_soldr_runtime(&mut cmd);
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

soldr_cli::timed_test!(
    standalone_compiler_shim_recovers_with_canonical_daemon_image,
    Duration::from_secs(120),
    {
        let cache_root = unique_temp_dir("daemon-standalone-wrapper-cache");
        let home_root = unique_temp_dir("daemon-standalone-wrapper-home");
        let shim_dir = unique_temp_dir("daemon-standalone-wrapper-bin");
        let workspace = unique_temp_dir("daemon-standalone-wrapper-workspace");
        let _cleanup = DaemonCleanup {
            cache_root: cache_root.clone(),
            home_root: home_root.clone(),
        };

        let compiler_shim = shim_dir.join(if cfg!(windows) { "rustc.exe" } else { "rustc" });
        fs::copy(common::soldr_bin(), &compiler_shim).expect("copy compiler-named soldr shim");
        let source = workspace.join("probe.rs");
        let out_dir = workspace.join("out");
        fs::create_dir_all(&out_dir).expect("create rustc out dir");
        fs::write(&source, "pub fn answer() -> u8 { 42 }\n").expect("write rust source");

        let mut cmd = Command::new(&compiler_shim);
        cmd.args([
            common::rustup_which("rustc"),
            "--crate-name".to_string(),
            "standalone_wrapper_probe".to_string(),
            "--crate-type".to_string(),
            "lib".to_string(),
            "--emit".to_string(),
            "metadata".to_string(),
            source.display().to_string(),
            "--out-dir".to_string(),
            out_dir.display().to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        for (key, value) in isolated_env(&cache_root, &home_root) {
            cmd.env(key, value);
        }
        cmd.env("RUNNING_PROCESS_DISABLE", "1")
            .env("SOLDR_CACHE_ENABLED", "1")
            .env("SOLDR_DAEMON_REQUIRED", "1")
            .env("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "40000")
            .env("SOLDR_COMPILE_REPLY_TIMEOUT_SECS", "60")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("SOLDR_INTERNAL_DAEMON_EXE")
            .env_remove("SOLDR_ORIGINAL_EXE")
            .env_remove("SOLDR_RELOCATED_EXE");

        let mut child = cmd.spawn().expect("spawn standalone compiler shim");
        if child
            .wait_timeout(Duration::from_secs(90))
            .expect("wait for standalone compiler shim")
            .is_none()
        {
            let _ = child.kill();
            let output = child.wait_with_output().expect("collect timed-out wrapper");
            panic!(
                "standalone compiler shim timed out\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = child.wait_with_output().expect("collect compiler shim");
        assert!(
            output.status.success(),
            "standalone compiler shim failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let paths = SoldrPaths::with_root(cache_root.clone());
        let (_pid, daemon_exe) =
            soldr_cli::daemon::lifecycle::read_pid_file(&paths).expect("daemon PID publication");
        assert_eq!(
            daemon_exe.file_stem().and_then(std::ffi::OsStr::to_str),
            Some("soldr-daemon"),
            "compiler recovery must never leave a rustc-named daemon: {}",
            daemon_exe.display()
        );
        assert!(
            shim_dir
                .join(if cfg!(windows) {
                    "soldr-daemon.exe"
                } else {
                    "soldr-daemon"
                })
                .is_file(),
            "standalone wrapper recovery must materialize the canonical daemon alias"
        );
    }
);

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
