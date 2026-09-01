//! soldr#2442 slice 3 — multi-process kill-matrix (Unix / Docker Linux).
//!
//! The broker-fronted design's correctness contract: killing or upgrading
//! any process has bounded, tested behavior. The in-process halves already
//! exist (`race_against_disconnect` for client EOF mid-compile,
//! zccache#1363's kill_on_drop + PDEATHSIG for compiler children); these tests
//! prove the Soldr adapter wiring end to end with real SIGKILLs:
//!
//! - daemon killed → only that route's generation is invalidated; the
//!   broker survives untouched and the next start launches one replacement;
//! - broker killed → the next front door brings up exactly one new broker.
//!
//! Generic multi-route isolation and concurrent single-flight replacement are
//! owned by running-process 4.10.9's
//! `backend_crash_concurrent_reconnects_launch_one_replacement_without_disturbing_other_instance`.
//! Keep this file as the small Soldr route/CLI adapter matrix, not a second
//! substrate conformance suite.
//!
//! Unix-gated at runtime (the platform-cfg boundary lives in
//! soldr-platform): the matrix drives `kill -9` and pgrep-style process
//! inspection, so on Windows every test returns immediately — the Windows
//! containment story is job-object-based and is exercised by the daemon
//! suites' own lifecycle tests.

use crate::common;

use soldr_cli::core::SoldrPaths;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const READY_DEADLINE: Duration = Duration::from_secs(60);

/// Runtime Unix gate (no host `#[cfg]` outside soldr-platform).
fn skip_on_windows() -> bool {
    matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    )
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn isolated_env(cache_root: &Path, home_root: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("SOLDR_CACHE_DIR", cache_root.as_os_str().to_os_string()),
        ("HOME", home_root.as_os_str().to_os_string()),
        ("USERPROFILE", home_root.as_os_str().to_os_string()),
    ]
}

fn soldr_command(args: &[&str], cache_root: &Path, home_root: &Path) -> Command {
    let mut cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
    cmd.args(args);
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    cmd.env(
        soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
        common::isolated_daemon::isolated_daemon_executable(
            &common::soldr_daemon_bin(),
            cache_root,
        ),
    );
    cmd.stdin(Stdio::null());
    cmd
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    soldr_command(args, cache_root, home_root)
        .output()
        .expect("failed to run soldr")
}

fn status_reports_running(cache_root: &Path, home_root: &Path) -> bool {
    let out = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|body| body["running"].as_bool())
        .unwrap_or(false)
}

fn daemon_pid(cache_root: &Path) -> Option<u32> {
    soldr_cli::daemon::lifecycle::read_route_claim_identity(&SoldrPaths::with_root(
        cache_root.to_path_buf(),
    ))
    .map(|(pid, _)| pid)
}

fn wait_for_running(cache_root: &Path, home_root: &Path) -> bool {
    let deadline = Instant::now() + READY_DEADLINE;
    while Instant::now() < deadline {
        if status_reports_running(cache_root, home_root) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn sigkill(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

fn process_is_alive(pid: u32) -> bool {
    soldr_platform::process::inspect::is_alive(pid)
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_is_alive(pid)
}

/// Broker pids for an isolated home, by argv (the broker runs from the
/// home's staged image path, so the home path is a unique argv marker).
fn broker_pids(home_root: &Path) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-f", &format!("{}/.soldr/broker", home_root.display())])
        .output()
        .expect("run pgrep");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

struct Fixture {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        Self {
            cache_root: unique_temp_dir(&format!("{label}-cache")),
            home_root: unique_temp_dir(&format!("{label}-home")),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root);
        // This isolated HOME owns its staged broker image and service-definition
        // directory. `broker stop` acknowledges before process exit, so wait
        // before deleting either tree; otherwise the still-live broker serves
        // the next test from a definition directory we just removed.
        let broker_pids = broker_pids(&self.home_root);
        let _ = run_soldr(&["broker", "stop"], &self.cache_root, &self.home_root);
        for pid in broker_pids {
            if !wait_for_process_exit(pid, Duration::from_secs(5)) {
                sigkill(pid);
                let _ = wait_for_process_exit(pid, Duration::from_secs(5));
            }
        }
        let _ = fs::remove_dir_all(&self.cache_root);
        let _ = fs::remove_dir_all(&self.home_root);
    }
}

#[test]
fn daemon_kill_invalidates_only_its_route_and_one_replacement_launches() {
    if skip_on_windows() {
        return;
    }
    let fx = Fixture::new("killmatrix-daemon");
    let start = run_soldr(&["daemon", "start"], &fx.cache_root, &fx.home_root);
    assert!(
        start.status.success() && wait_for_running(&fx.cache_root, &fx.home_root),
        "initial daemon start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let old_daemon = daemon_pid(&fx.cache_root).expect("route claim carries the daemon pid");
    let brokers_before = broker_pids(&fx.home_root);
    assert_eq!(
        brokers_before.len(),
        1,
        "exactly one broker before the kill"
    );

    sigkill(old_daemon);
    assert!(
        wait_for_process_exit(old_daemon, Duration::from_secs(10)),
        "SIGKILLed daemon must exit"
    );

    // The broker is untouched by its route's daemon dying (soldr#2549:
    // generations belong to the daemon; the broker is a stable singleton).
    assert_eq!(
        broker_pids(&fx.home_root),
        brokers_before,
        "the broker must survive its daemon's death"
    );

    let restart = run_soldr(&["daemon", "start"], &fx.cache_root, &fx.home_root);
    assert!(
        restart.status.success() && wait_for_running(&fx.cache_root, &fx.home_root),
        "restart after kill failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    let new_daemon = daemon_pid(&fx.cache_root).expect("replacement route claim");
    assert_ne!(
        new_daemon, old_daemon,
        "a replacement generation, not the corpse"
    );
    assert_eq!(
        broker_pids(&fx.home_root),
        brokers_before,
        "the same broker owns the replacement route"
    );
}

#[test]
fn broker_kill_is_recovered_by_the_next_front_door_with_one_replacement() {
    if skip_on_windows() {
        return;
    }
    let fx = Fixture::new("killmatrix-broker");
    let start = run_soldr(&["daemon", "start"], &fx.cache_root, &fx.home_root);
    assert!(
        start.status.success() && wait_for_running(&fx.cache_root, &fx.home_root),
        "initial daemon start failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    let brokers_before = broker_pids(&fx.home_root);
    assert_eq!(brokers_before.len(), 1, "one broker after bringup");

    sigkill(brokers_before[0]);
    assert!(
        wait_for_process_exit(brokers_before[0], Duration::from_secs(10)),
        "SIGKILLed broker must exit"
    );

    // The next front door launches exactly one new broker and the route
    // comes back (daemon routes are re-adopted from their verified claims).
    let deadline = Instant::now() + READY_DEADLINE;
    let mut recovered = false;
    while Instant::now() < deadline {
        if status_reports_running(&fx.cache_root, &fx.home_root) {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(recovered, "status must recover after a broker kill");
    let brokers_after = broker_pids(&fx.home_root);
    assert_eq!(
        brokers_after.len(),
        1,
        "exactly one replacement broker: {brokers_after:?}"
    );
    assert_ne!(brokers_after[0], brokers_before[0]);
}

/// Keep the expensive real-process adapter matrix explicit. Generic route
/// isolation/replacement belongs to running-process; adding another Soldr
/// process test here requires updating this guard and the ownership table in
/// `docs/CONTRIBUTING_TESTS.md` deliberately.
#[test]
fn generic_running_process_ownership_does_not_return_to_soldr_matrix() {
    let this_file = include_str!("cli_kill_matrix.rs");
    assert_eq!(
        this_file
            .lines()
            .filter(|line| line.trim() == "#[test]")
            .count(),
        3,
        "keep only two Soldr adapter processes plus this inventory guard"
    );
    for upstream_owned in [
        concat!(
            "fn two_roots_killing_one_daemon_",
            "never_disrupts_the_other"
        ),
        concat!(
            "fn concurrent_restarts_after_a_kill_",
            "converge_on_one_replacement"
        ),
    ] {
        assert!(
            !this_file.contains(upstream_owned),
            "{upstream_owned} is owned by running-process lifecycle_process_conformance"
        );
    }
    assert!(
        !include_str!("cli_broker_resurrection.rs").contains(concat!(
            "fn issue_2476_sixty_four_process_",
            "stampede_binds_one_broker"
        )),
        "generic singleton stampede coverage is owned by running-process"
    );
}
