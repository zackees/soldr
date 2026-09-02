//! soldr#3059: SIGTERM must take a fast, bounded-in-milliseconds exit path
//! instead of the graceful drain issue #1286 originally wired it into.
//!
//! Two properties, each against a real `soldr-daemon` process:
//!
//! - [`sigterm_takes_the_fast_exit_path`]: sending SIGTERM produces the
//!   lifecycle end-of-stream marker (`died-signal-fast`), a stderr line
//!   naming the signal and pid, exit code 1, and an elapsed
//!   signal-to-exit time bounded well under the old 240s graceful-drain
//!   watchdog grace.
//! - [`ordinary_daemon_stop_still_exits_cleanly_without_the_fast_marker`]:
//!   an explicit `soldr daemon stop` is unaffected -- it still exits 0 and
//!   still records `died-shutdown`, never the fast-path marker. This
//!   negative assertion is what actually proves the two paths are
//!   separate, not merely that SIGTERM happens to look fast.
//!
//! Both spawn the daemon directly via `isolated_daemon_command` rather
//! than the shared `IsolatedDaemon` test helper: that helper's `Drop` impl
//! sends its own `daemon stop` and would race a test that signals the
//! child itself.

#![allow(clippy::print_stdout)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;
use soldr_cli::cache_lib::daemon_lifecycle_log_path;
use soldr_cli::core::SoldrPaths;
use wait_timeout::ChildExt;

/// Headroom for a loaded CI host. This is the *bound*, not the claim of
/// "fast" -- the measured elapsed time, printed by the test below, is the
/// number that actually answers "how fast". It is chosen to sit orders of
/// magnitude under the 240s graceful-drain watchdog grace
/// (`server_runtime::SHUTDOWN_WATCHDOG_GRACE`) this path exists to avoid
/// racing, while still tolerating process-spawn and OS-scheduling jitter on
/// a contended CI runner.
const FAST_EXIT_BUDGET: Duration = Duration::from_secs(5);

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

/// A foreground isolated daemon with its stderr piped (not nulled) so the
/// fast-exit test can read the diagnostic line the owner asked for.
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
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(90);
        let route_claim = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let mut ready = false;
        while Instant::now() < deadline {
            if route_claim.exists() {
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

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("daemon child present").id()
    }

    /// Take ownership of the child process for a test that signals or
    /// waits on it directly. After this, `Drop` finds nothing to stop.
    fn take_child(&mut self) -> Child {
        self.child.take().expect("daemon child already taken")
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

/// soldr#3059: SIGTERM must produce the end-of-stream marker, exit
/// non-zero, and say so on stderr -- all within milliseconds, not the
/// graceful drain's unbounded steps.
#[test]
fn sigterm_takes_the_fast_exit_path() {
    let cache_root = unique_temp_dir("fast-sigterm-cache");
    let home_root = unique_temp_dir("fast-sigterm-home");
    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);
    let pid = daemon.pid();
    let mut child = daemon.take_child();

    let started = Instant::now();
    soldr_platform::process::terminate::signal_pid(pid, false)
        .expect("send SIGTERM to the isolated daemon");
    let status = child
        .wait_timeout(FAST_EXIT_BUDGET)
        .expect("wait on the signalled daemon")
        .unwrap_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "soldr-daemon (pid {pid}) did not exit within {FAST_EXIT_BUDGET:?} of SIGTERM \
                 -- the fast-exit path must be bounded in milliseconds, not merely under 5s"
            );
        });
    let elapsed = started.elapsed();
    // The number soldr#3059 actually asked for: report it regardless of
    // pass/fail so a CI log always carries the measurement.
    println!(
        "soldr#3059 SIGTERM-to-exit measured elapsed: {elapsed:?} (pid {pid}, budget {FAST_EXIT_BUDGET:?})"
    );

    assert_eq!(
        status.code(),
        Some(1),
        "a fast SIGTERM exit must report exit code 1, got {status:?}"
    );
    assert!(
        elapsed < FAST_EXIT_BUDGET,
        "fast exit must be bounded well under {FAST_EXIT_BUDGET:?} even on a loaded CI host; \
         took {elapsed:?}"
    );

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("daemon stderr must have been piped")
        .read_to_string(&mut stderr)
        .expect("read daemon stderr");
    assert!(
        stderr.contains("SIGTERM") && stderr.contains(&pid.to_string()),
        "stderr must name the signal and the pid: {stderr:?}"
    );

    let paths = SoldrPaths::with_root(cache_root.clone());
    let lifecycle = std::fs::read_to_string(daemon_lifecycle_log_path(&paths))
        .expect("lifecycle journal must exist after a fast SIGTERM exit");
    assert!(
        lifecycle.contains("\"event\":\"died-signal-fast\""),
        "lifecycle journal must carry the fast-exit end-of-stream marker: {lifecycle}"
    );

    drop(daemon);
}

/// The negative half of the same property (soldr#3059): an explicit
/// `soldr daemon stop` must be completely unaffected by the SIGTERM
/// fast-exit path -- still the graceful drain, still exit 0, and it must
/// never write the fast-path marker. Without this, a test that only checks
/// SIGTERM's own behavior cannot tell "SIGTERM got its own path" from
/// "every shutdown got faster."
#[test]
fn ordinary_daemon_stop_still_exits_cleanly_without_the_fast_marker() {
    let cache_root = unique_temp_dir("graceful-stop-cache");
    let home_root = unique_temp_dir("graceful-stop-home");
    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);
    let mut child = daemon.take_child();

    let out = run_soldr(&["daemon", "stop"], &cache_root, &home_root);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "daemon stop must succeed; stdout: {stdout}; stderr: {stderr}"
    );

    let status = child
        .wait_timeout(Duration::from_secs(10))
        .expect("wait on the stopped daemon")
        .unwrap_or_else(|| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("soldr-daemon did not exit after an explicit `daemon stop`");
        });
    assert_eq!(
        status.code(),
        Some(0),
        "a graceful `daemon stop` must exit 0, got {status:?}"
    );

    let paths = SoldrPaths::with_root(cache_root.clone());
    let lifecycle = std::fs::read_to_string(daemon_lifecycle_log_path(&paths))
        .expect("lifecycle journal must exist after a graceful stop");
    assert!(
        lifecycle.contains("\"event\":\"died-shutdown\""),
        "a graceful stop must still record died-shutdown: {lifecycle}"
    );
    assert!(
        !lifecycle.contains("died-signal-fast"),
        "an ordinary `daemon stop` must never take the SIGTERM fast-exit path: {lifecycle}"
    );

    drop(daemon);
}
