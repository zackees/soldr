//! soldr#1495 regression coverage: a soldr-daemon that advertises a
//! stale package version (via the `SOLDR_TEST_DAEMON_FAKE_PKG_VERSION`
//! seam) is detected as not-current by a current-version client and
//! displaced, while a current-version daemon is left untouched.

#![allow(clippy::print_stdout)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::broker_discovery::read_claimed_service_version;
use soldr_cli::daemon::lifecycle::{
    displace_stale_daemon, is_live, is_live_current_version, preflight_displace_stale_daemon,
    stale_daemon_occupies_endpoint,
};
use soldr_cli::timed_test;

mod common;

// The daemon subprocess env and the in-process `SoldrPaths::new()` must
// see the same SOLDR_CACHE_DIR/HOME, and these are process-global. Every
// test here mutates them, so serialize.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvScope {
    keys: Vec<(&'static str, Option<OsString>)>,
}

impl EnvScope {
    fn apply(vars: &[(&'static str, &Path)]) -> Self {
        let mut keys = Vec::new();
        for (k, v) in vars {
            keys.push((*k, std::env::var_os(k)));
            std::env::set_var(k, v.as_os_str());
        }
        // Never let an ambient wrapper leak into the probe.
        keys.push(("RUSTC_WRAPPER", std::env::var_os("RUSTC_WRAPPER")));
        std::env::remove_var("RUSTC_WRAPPER");
        Self { keys }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, prior) in self.keys.drain(..) {
            match prior {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn isolated_env(cache_root: &Path, home_root: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("SOLDR_CACHE_DIR", cache_root.as_os_str().to_os_string()),
        ("HOME", home_root.as_os_str().to_os_string()),
        ("USERPROFILE", home_root.as_os_str().to_os_string()),
    ]
}

/// Run a `soldr <args>` subprocess in the daemon's isolated root.
fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path, fake_version: Option<&str>) {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in isolated_env(cache_root, home_root) {
        cmd.env(k, v);
    }
    if let Some(v) = fake_version {
        cmd.env("SOLDR_TEST_DAEMON_FAKE_PKG_VERSION", v);
    }
    cmd.env_remove("RUSTC_WRAPPER");
    let _ = cmd.status();
}

struct SpawnedDaemon {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl SpawnedDaemon {
    /// Spawn a real, DETACHED soldr-daemon in an isolated root (via
    /// `soldr daemon start`, which double-detaches like production so the
    /// daemon is reparented to init and reaped on exit — a directly-owned
    /// child would linger as a zombie after graceful shutdown and defeat
    /// the liveness check). Optionally claims a faked package version.
    fn spawn(fake_version: Option<&str>) -> Self {
        let cache_root = unique_temp_dir("displace-cache");
        let home_root = unique_temp_dir("displace-home");
        // `soldr daemon start` requests a detached spawn and returns.
        run_soldr(
            &["daemon", "start", "--idle-timeout", "120"],
            &cache_root,
            &home_root,
            fake_version,
        );
        let this = Self {
            cache_root,
            home_root,
        };
        assert!(
            this.wait_until_live(Duration::from_secs(15)),
            "daemon never became live"
        );
        this
    }

    /// Build a `SoldrPaths` for this daemon's root by installing its env
    /// process-globally (caller holds ENV_LOCK) and reading it back the
    /// same way production code does.
    fn paths(&self) -> (EnvScope, SoldrPaths) {
        let scope = EnvScope::apply(&[
            ("SOLDR_CACHE_DIR", &self.cache_root),
            ("HOME", &self.home_root),
            ("USERPROFILE", &self.home_root),
        ]);
        let paths = SoldrPaths::new().expect("resolve paths");
        (scope, paths)
    }

    /// The live daemon's PID via the version-blind occupancy probe.
    fn pid(&self) -> Option<u32> {
        let (_scope, paths) = self.paths();
        stale_daemon_occupies_endpoint(&paths)
    }

    fn wait_until_live(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let live = {
                let (_scope, paths) = self.paths();
                is_live(&paths).is_some() && read_claimed_service_version(&paths).is_some()
            };
            if live {
                return true;
            }
            std::thread::sleep(Duration::from_millis(75));
        }
        false
    }

    fn is_gone(&self) -> bool {
        let (_scope, paths) = self.paths();
        is_live(&paths).is_none()
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        // Best-effort teardown of any daemon still holding the endpoint.
        run_soldr(&["daemon", "stop"], &self.cache_root, &self.home_root, None);
    }
}

timed_test!(
    stale_version_daemon_is_displaced_by_current_client,
    Duration::from_secs(60),
    {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let daemon = SpawnedDaemon::spawn(Some("0.0.0-stale"));
        let old_pid = daemon.pid().expect("stale daemon has a pid");

        {
            let (_scope, paths) = daemon.paths();

            // The daemon is live, but advertises a stale version — so a
            // current-version client sees it as NOT the daemon it wants.
            assert!(is_live(&paths).is_some(), "stale daemon should be live");
            assert_eq!(
                read_claimed_service_version(&paths).as_deref(),
                Some("0.0.0-stale"),
                "daemon should have published the faked version claim",
            );
            assert!(
                is_live_current_version(&paths).is_none(),
                "a stale-version daemon must not count as the current version",
            );
            assert!(
                stale_daemon_occupies_endpoint(&paths).is_some(),
                "the stale daemon should be seen as occupying the endpoint",
            );

            // Displace it. Same PROTOCOL_VERSION here (one binary), so the
            // graceful wire Shutdown evicts it; the verified-PID kill
            // fallback is unit-tested separately.
            assert!(
                displace_stale_daemon(&paths),
                "displacement should free the endpoint",
            );

            // The endpoint is now free and the stale claim is gone.
            assert!(
                is_live(&paths).is_none(),
                "the displaced daemon must no longer be live",
            );
            assert!(
                read_claimed_service_version(&paths).is_none(),
                "the stale version claim must be removed on displacement",
            );
        }

        // The endpoint is free (the daemon, detached, was reaped by init
        // on exit).
        let gone_deadline = Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while Instant::now() < gone_deadline {
            if daemon.is_gone() {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "old daemon pid {old_pid} should have exited");
    }
);

timed_test!(
    current_version_daemon_is_not_displaced,
    Duration::from_secs(60),
    {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let daemon = SpawnedDaemon::spawn(None);
        let pid = daemon.pid().expect("current daemon has a pid");

        {
            let (_scope, paths) = daemon.paths();

            assert_eq!(
                is_live_current_version(&paths),
                Some(pid),
                "a current-version daemon must be recognized as current",
            );

            // The front-door preflight must leave a current-version daemon alone.
            preflight_displace_stale_daemon(&paths);
            assert_eq!(
                is_live_current_version(&paths),
                Some(pid),
                "preflight must not displace a current-version daemon",
            );
        }
    }
);
