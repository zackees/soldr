//! Verify the wrapper-side fast path (daemon present → IPC) and the
//! fallback path (daemon absent → direct redb) both populate the
//! target registry with the workspace `target/` row.

#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common;
use soldr_cli::cache_lib::target_registry::TargetRegistry;
use soldr_cli::daemon::client;
use soldr_cli::daemon::lifecycle;
use soldr_cli::daemon::protocol::Request;

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

fn direct_sock(root: &Path) -> PathBuf {
    common::isolated_daemon::isolated_daemon_control_endpoint(&soldr_daemon_bin(), root)
}

struct EnvScope {
    keys: Vec<&'static str>,
    prior: Vec<Option<OsString>>,
}

impl EnvScope {
    fn set(pairs: &[(&'static str, &Path)]) -> Self {
        let mut prior = Vec::with_capacity(pairs.len());
        let mut keys = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            prior.push(std::env::var_os(k));
            std::env::set_var(k, v);
            keys.push(*k);
        }
        Self { keys, prior }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, p) in self.keys.iter().zip(self.prior.iter()) {
            match p {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

struct DaemonProc {
    child: Option<Child>,
    cache_root: PathBuf,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd =
            common::isolated_daemon::isolated_daemon_command(&soldr_daemon_bin(), cache_root);
        // Capture stderr to a file: when a lane-specific failure appears (the
        // aarch64-darwin target-run lane lost every RecordTargetTouch write
        // while status round-trips worked), the daemon's own diagnostics are
        // the only evidence, and Stdio::null() was discarding them.
        let stderr_log = std::fs::File::create(daemon_stderr_path(cache_root))
            .expect("create daemon stderr log");
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_log));
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(40);
        let pid_path = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let sock = direct_sock(cache_root);
        let mut status_ok = false;
        while Instant::now() < deadline {
            if pid_path.exists() && client::status(&sock).is_ok() {
                status_ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            status_ok,
            "daemon control endpoint never answered status within 40s; daemon stderr:\n{}",
            daemon_stderr_tail(cache_root)
        );
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
        }
    }
}

fn daemon_stderr_path(cache_root: &Path) -> PathBuf {
    cache_root.join("daemon-stderr.log")
}

fn daemon_stderr_tail(cache_root: &Path) -> String {
    let text = std::fs::read_to_string(daemon_stderr_path(cache_root)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(40);
    lines[start..].join("\n")
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = client::shutdown(&direct_sock(&self.cache_root));
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

fn registry_row_exists(cache_root: &Path, target_path: &Path) -> Option<i64> {
    // Tolerant of a lock-contended open: the live daemon's per-write handle
    // takes the same exclusive redb lock, so a failed open during a poll
    // means "no observation this round", not a test failure.
    let db_path = cache_root.join("state.sqlite3");
    let registry = TargetRegistry::open(&db_path).ok()?;
    registry
        .get(target_path)
        .ok()
        .flatten()
        .map(|row| row.last_used)
}

#[test]
fn unavailable_daemon_does_not_open_state_db() {
    let cache_root = unique_temp_dir("target-touch-fallback-cache");
    let home_root = unique_temp_dir("target-touch-fallback-home");
    let target = cache_root.join("dev").join("workspace").join("target");
    std::fs::create_dir_all(&target).expect("seed target dir");

    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ]);
    let paths = soldr_cli::core::SoldrPaths::new().expect("paths");

    let db_path = cache_root.join("state.sqlite3");
    client::record_target_touch_or_fallback(&paths, &target);

    assert!(
        !db_path.exists(),
        "unavailable daemon must not cause the client to create or open {}",
        db_path.display()
    );
}

#[test]
fn daemon_path_writes_via_ipc_when_available() {
    let cache_root = unique_temp_dir("target-touch-daemon-cache");
    let home_root = unique_temp_dir("target-touch-daemon-home");
    let target = cache_root.join("dev").join("workspace").join("target");
    std::fs::create_dir_all(&target).expect("seed target dir");

    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);

    // Derive the control endpoint BEFORE EnvScope swaps HOME: when the
    // executable-scoped socket path overflows `sun_path` (long temp dirs —
    // macOS `/var/folders/...`, Docker harness roots), the derivation falls
    // back to a path under the ambient HOME's broker dir. The daemon bound
    // using the spawn-time ambient env, so the client must derive under that
    // same env or it dials a socket nothing listens on — which made this
    // test fail deterministically on exactly those hosts while passing on
    // short-`/tmp` Linux runners.
    let sock = direct_sock(&cache_root);

    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ]);
    let paths = soldr_cli::core::SoldrPaths::new().expect("paths");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if lifecycle::is_live(&paths).is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        lifecycle::is_live(&paths).is_some(),
        "daemon never published a live route claim"
    );

    let mut submitted = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match client::submit_fire_and_forget(
            &sock,
            &Request::RecordTargetTouch {
                path: target.display().to_string(),
                unix_seconds: 1_700_000_000,
            },
        ) {
            Ok(()) => {
                submitted = true;
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    assert!(submitted, "fire-and-forget submit never succeeded");

    // Converge on the row BEFORE initiating shutdown, resubmitting on every
    // iteration. Two hazards make a submit-once-then-wait shape flaky and
    // both bit the darwin target-run lanes:
    //
    // * The old fixed 200ms sleep raced the daemon's accept -> read ->
    //   spawn_blocking -> redb-open pipeline, and the 3s-then-kill teardown
    //   below could destroy the daemon before the write landed.
    // * A single fire-and-forget write is silently dropped if the daemon's
    //   per-write `TargetRegistry::open` loses the exclusive redb lock race
    //   -- including to THIS test's own `registry_row_exists` probe, which
    //   takes the same lock. Errors are silent by design on that path, so
    //   the only convergent shape is submit-check-repeat: the upsert is
    //   idempotent (same path, same timestamp), and any write lost to a
    //   lock collision is replayed by the next iteration.
    let mut row = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let _ = client::submit_fire_and_forget(
            &sock,
            &Request::RecordTargetTouch {
                path: target.display().to_string(),
                unix_seconds: 1_700_000_000,
            },
        );
        std::thread::sleep(Duration::from_millis(150));
        row = registry_row_exists(&cache_root, &target);
        if row.is_some() {
            break;
        }
    }

    let _ = client::shutdown(&sock);
    if let Some(mut child) = daemon.child.take() {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    // Failure triage aid: distinguish "this platform/path cannot take the
    // write at all" from "the daemon never processed the frame" by writing a
    // sentinel row directly from the test process.
    let direct_probe = TargetRegistry::open(&cache_root.join("state.sqlite3"))
        .map_err(|error| error.to_string())
        .and_then(|registry| {
            registry
                .upsert_with_time(Path::new("probe-sentinel"), 1)
                .map_err(|error| error.to_string())
        });
    assert!(
        row.is_some(),
        "daemon never wrote the target registry row for {}; \
         direct write from the test process: {direct_probe:?}; daemon stderr:\n{}",
        target.display(),
        daemon_stderr_tail(&cache_root)
    );
    assert_eq!(row, Some(1_700_000_000));
}
