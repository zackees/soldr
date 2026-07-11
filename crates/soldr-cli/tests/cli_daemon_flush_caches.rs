//! Issue #1286 (F1): `soldr cache flush` must checkpoint the
//! soldr-daemon's EMBEDDED zccache state (artifact index, depgraph
//! snapshot, metadata cache) to disk via `Request::FlushCaches`.
//!
//! Before the fix, `cache flush` / `cache shutdown` only quiesced the
//! managed zccache daemon (the C/C++ side); the embedded rustc-side
//! state stayed memory-only until a graceful daemon exit, so `soldr
//! save` archives taken from a live daemon restored with zero rustc
//! hits (the cold-tar-untar-warm 1.00x-speedup bug).

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::timed_test;

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
    let parent = soldr.parent().expect("parent");
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn run_soldr(args: &[&str], cache_root: &Path, home_root: &Path) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    cmd.args(args)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
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
        let mut cmd = Command::new(soldr_daemon_bin());
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(40);
        let pid_file = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("daemon.pid");
        while Instant::now() < deadline {
            if pid_file.exists() {
                let status = run_soldr(&["daemon", "status", "--json"], cache_root, home_root);
                if status.status.success()
                    && serde_json::from_slice::<serde_json::Value>(&status.stdout)
                        .ok()
                        .and_then(|body| body["running"].as_bool())
                        .unwrap_or(false)
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
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

timed_test!(
    cache_flush_checkpoints_embedded_state,
    Duration::from_secs(120),
    {
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
        assert!(
            stdout.contains("embedded zccache state flushed"),
            "cache flush notes must confirm the embedded checkpoint ran \
             (Request::FlushCaches -> Ack); stdout: {stdout}"
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
);
