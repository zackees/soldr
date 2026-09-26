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
//! soldr-platform): the matrix drives `kill -9`, so on Windows every test
//! returns immediately — the Windows
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

/// soldr#3374 keys route claims per daemon generation. This test process has
/// no `SOLDR_BROKER_SERVICE`, and deriving the key from `current_exe()` names
/// the test binary's generation rather than the fixture's, so read the claim
/// under the route the front door registered for the isolated daemon image.
fn daemon_pid(cache_root: &Path) -> Option<u32> {
    use soldr_cli::daemon::backend_handle_adoption::{
        broker_service_name_for, with_generation_key,
    };
    let paths = SoldrPaths::with_root(cache_root.to_path_buf());
    let daemon = common::isolated_daemon::isolated_daemon_executable(
        &common::soldr_daemon_bin(),
        cache_root,
    );
    let service = broker_service_name_for(&paths, &daemon).ok()?;
    with_generation_key(&service, || {
        soldr_cli::daemon::lifecycle::read_route_claim_identity(&paths)
    })
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

/// The stable broker's pid for an isolated home, as the broker itself reports
/// it through `soldr broker status --json`.
///
/// soldr#3136: this used to be `pgrep -f "<home>/.soldr/broker"`. Inside the
/// macOS Recovery replay guest that matched nothing, so both tests failed with
/// `left: 0` against a broker that was running and serving the route. Asking
/// the broker is portable and names the process that owns the endpoint rather
/// than any process whose argv happens to mention the path.
fn broker_pid(cache_root: &Path, home_root: &Path) -> Option<u32> {
    let out = run_soldr(&["broker", "status", "--json"], cache_root, home_root);
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()?
        .get("broker_pid")?
        .as_u64()
        .filter(|pid| *pid != 0)
        .and_then(|pid| u32::try_from(pid).ok())
}

/// How many brokers have bound this home's stable endpoint, counted from the
/// spawn log every detached broker writes into. A second broker cannot bind
/// the endpoint the first one holds, so binds are the singleton count; the
/// resurrection and cold-start suites assert "exactly one broker" the same way.
fn broker_binds(home_root: &Path) -> usize {
    fs::read_to_string(home_root.join(".soldr/broker/broker-spawn.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("stable endpoint bound at"))
        .count()
}

/// [`broker_binds`] once it reaches `expected` or the deadline passes. The
/// log line is written by a detached process, so a snapshot taken the moment
/// status answers can trail it; waiting still reports an excess bind.
fn wait_for_broker_binds(home_root: &Path, expected: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let binds = broker_binds(home_root);
        if binds >= expected || Instant::now() >= deadline {
            return binds;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
        let pid = broker_pid(&self.cache_root, &self.home_root);
        let _ = run_soldr(&["broker", "stop"], &self.cache_root, &self.home_root);
        if let Some(pid) = pid {
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
    let broker_before =
        broker_pid(&fx.cache_root, &fx.home_root).expect("a broker answers status after bringup");
    assert_eq!(
        wait_for_broker_binds(&fx.home_root, 1),
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
        broker_pid(&fx.cache_root, &fx.home_root),
        Some(broker_before),
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
        broker_pid(&fx.cache_root, &fx.home_root),
        Some(broker_before),
        "the same broker owns the replacement route"
    );
    assert_eq!(
        broker_binds(&fx.home_root),
        1,
        "replacing the daemon must not bind a second broker"
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
    let broker_before =
        broker_pid(&fx.cache_root, &fx.home_root).expect("a broker answers status after bringup");
    assert_eq!(
        wait_for_broker_binds(&fx.home_root, 1),
        1,
        "one broker after bringup"
    );

    sigkill(broker_before);
    assert!(
        wait_for_process_exit(broker_before, Duration::from_secs(10)),
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
    let broker_after =
        broker_pid(&fx.cache_root, &fx.home_root).expect("the replacement broker answers status");
    assert_ne!(
        broker_after, broker_before,
        "a replacement broker, not the corpse"
    );
    assert_eq!(
        wait_for_broker_binds(&fx.home_root, 2),
        2,
        "exactly one replacement broker bound after the kill"
    );
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
