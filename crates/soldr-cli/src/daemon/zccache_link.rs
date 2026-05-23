//! Linked zccache lifecycle (Phase 3). When the soldr-daemon's
//! session has registered a zccache daemon PID via `LinkZccache`, the
//! soldr-daemon's own shutdown (explicit RPC, signal, OR idle timeout)
//! runs `zccache stop` against that PID before exiting.

use crate::core::SoldrPaths;
use crate::daemon::db;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Best-effort: if a linked zccache PID is recorded, spawn
/// `zccache stop` and wait up to 5 s. All errors are swallowed —
/// the soldr-daemon exits regardless so a hung zccache can't keep
/// soldr-daemon alive.
pub fn stop_linked_zccache(paths: &SoldrPaths) {
    let db_path = db::db_path(paths);
    let Ok(Some(_pid)) = db::get_linked_zccache_pid(&db_path) else {
        return;
    };
    let Some(zccache_bin) = resolve_zccache_binary(paths) else {
        let _ = db::set_linked_zccache_pid(&db_path, None);
        return;
    };

    let mut cmd = Command::new(&zccache_bin);
    cmd.arg("stop")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else {
        let _ = db::set_linked_zccache_pid(&db_path, None);
        return;
    };
    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = db::set_linked_zccache_pid(&db_path, None);
}

/// Resolve the zccache binary path the daemon should use to issue
/// `stop`. Mirrors the precedence chain the CLI uses, minus any
/// network fetch — if no cached binary exists, no stop attempt is made.
fn resolve_zccache_binary(paths: &SoldrPaths) -> Option<std::path::PathBuf> {
    // SOLDR_ZCCACHE_BIN env var (test/debug override) wins outright.
    if let Some(env_bin) = std::env::var_os(crate::cache_lib::ZCCACHE_BINARY_ENV_VAR) {
        let p = std::path::PathBuf::from(env_bin);
        if p.exists() {
            return Some(p);
        }
    }
    // Cache-pinned install: ~/.soldr/bin/zccache-pinned/zccache[.exe]
    let pinned = paths.pinned_bin.join("zccache-pinned").join(zccache_stem());
    if pinned.exists() {
        return Some(pinned);
    }
    // Managed install dir under ~/.soldr/bin/zccache-<version>/. Walk
    // the bin/ dir and pick any zccache* directory's binary; without
    // version pinning at hand, the most-recently-modified wins so the
    // last install drives our stop.
    let bin_dir = &paths.bin;
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(bin_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join(zccache_stem());
            if !candidate.exists() {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .unwrap_or(std::time::UNIX_EPOCH);
            match &best {
                Some((current, _)) if mtime <= *current => {}
                _ => best = Some((mtime, candidate)),
            }
        }
    }
    let resolved = best.map(|(_, p)| p);
    let _ = bin_dir; // touch
    let _: Option<&Path> = None; // silence unused warning when feature gates flip
    resolved
}

fn zccache_stem() -> &'static str {
    if cfg!(windows) {
        "zccache.exe"
    } else {
        "zccache"
    }
}
