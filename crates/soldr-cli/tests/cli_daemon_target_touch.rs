//! Verify the wrapper-side fast path (daemon present → IPC) and the
//! fallback path (daemon absent → direct redb) both populate the
//! target registry with the workspace `target/` row.

#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::target_registry::TargetRegistry;
use soldr_cli::daemon::client;
use soldr_cli::daemon::lifecycle;
use soldr_cli::daemon::protocol::Request;
mod common;

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
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(40);
        let pid_path = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let sock = direct_sock(cache_root);
        while Instant::now() < deadline {
            if pid_path.exists() && client::status(&sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
        }
    }
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
    let db_path = cache_root.join("state.redb");
    let registry = TargetRegistry::open(&db_path).expect("open registry");
    registry
        .get(target_path)
        .expect("get registry row")
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

    let db_path = cache_root.join("state.redb");
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

    // The daemon persists the row immediately on processing (per-write redb
    // open in the dispatch handler), so it is observable without any
    // shutdown. Poll for it BEFORE initiating shutdown: the old fixed 200ms
    // sleep raced the daemon's accept -> read -> spawn_blocking -> redb-open
    // pipeline on slow hosts, and the 3s-then-kill teardown below could then
    // destroy the daemon before the write landed — the darwin target-run
    // lanes failed exactly here. A transient open-lock clash with the
    // daemon's own per-write handle reads as "no row yet" and simply polls
    // again.
    let mut row = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        row = registry_row_exists(&cache_root, &target);
        if row.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
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

    assert!(
        row.is_some(),
        "daemon never wrote the target registry row for {}",
        target.display()
    );
    assert_eq!(row, Some(1_700_000_000));
}
