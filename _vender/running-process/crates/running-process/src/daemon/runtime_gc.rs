//! Runtime directory GC for daemon trampoline directories.
//!
//! The Python `spawn_daemon()` helper creates per-daemon runtime directories
//! containing the renamed trampoline binary, its sidecar JSON, and a `daemon.pid`
//! file. This module lets `running-process-daemon` periodically prune dead
//! runtime directories once they have been stale for long enough.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, System};
use tracing::{debug, info, warn};

use crate::daemon::handlers::DaemonState;

const START_TIME_TOLERANCE_MS: u64 = 5_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeGcStats {
    pub scanned_dirs: usize,
    pub removed_dirs: usize,
    pub refreshed_sidecars: usize,
}

#[derive(Debug)]
struct RuntimeDirEntry {
    dir: PathBuf,
    sidecar_path: PathBuf,
    pid: Option<u32>,
    spawned_at_unix_ms: Option<u64>,
    last_seen_unix_ms: Option<u64>,
}

pub fn runtime_root() -> PathBuf {
    app_root().join("runtime")
}

fn app_root() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join("AppData").join("Local")))
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("running-process")
    }

    #[cfg(target_os = "macos")]
    {
        let base = dirs::home_dir()
            .map(|home| home.join("Library").join("Application Support"))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("running-process")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("running-process")
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

fn read_runtime_dir_entry(dir: &Path) -> Option<RuntimeDirEntry> {
    let name = dir.file_name()?.to_string_lossy();
    let sidecar_path = dir.join(format!("{name}.daemon.json"));
    let raw_sidecar = fs::read_to_string(&sidecar_path).ok()?;
    let sidecar: Value = serde_json::from_str(&raw_sidecar).ok()?;
    let sidecar_obj = sidecar.as_object()?;

    let pid = fs::read_to_string(dir.join("daemon.pid"))
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    let spawned_at_unix_ms = sidecar_obj
        .get("spawned_at_unix_ms")
        .and_then(Value::as_u64);
    let last_seen_unix_ms = sidecar_obj
        .get("last_seen_unix_ms")
        .and_then(Value::as_u64)
        .or(spawned_at_unix_ms);

    Some(RuntimeDirEntry {
        dir: dir.to_path_buf(),
        sidecar_path,
        pid,
        spawned_at_unix_ms,
        last_seen_unix_ms,
    })
}

fn process_matches(system: &mut System, pid: u32, spawned_at_unix_ms: Option<u64>) -> bool {
    let sys_pid = Pid::from_u32(pid);
    system.refresh_process_specifics(sys_pid, ProcessRefreshKind::new());
    let Some(process) = system.process(sys_pid) else {
        return false;
    };

    if let Some(spawned_at_unix_ms) = spawned_at_unix_ms {
        let process_start_ms = process.start_time() * 1000;
        if process_start_ms.abs_diff(spawned_at_unix_ms) > START_TIME_TOLERANCE_MS {
            return false;
        }
    }

    true
}

fn write_last_seen(sidecar_path: &Path, now_unix_ms: u64) -> Result<bool, String> {
    let raw = fs::read_to_string(sidecar_path)
        .map_err(|e| format!("read {} failed: {e}", sidecar_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {} failed: {e}", sidecar_path.display()))?;
    let Some(obj) = value.as_object_mut() else {
        return Err(format!("{} is not a JSON object", sidecar_path.display()));
    };
    if obj.get("last_seen_unix_ms").and_then(Value::as_u64) == Some(now_unix_ms) {
        return Ok(false);
    }
    obj.insert("last_seen_unix_ms".to_string(), Value::from(now_unix_ms));
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("serialize {} failed: {e}", sidecar_path.display()))?;
    fs::write(sidecar_path, rendered)
        .map_err(|e| format!("write {} failed: {e}", sidecar_path.display()))?;
    Ok(true)
}

fn prune_runtime_root_at(root: &Path, stale_after: Duration, now_unix_ms: u64) -> RuntimeGcStats {
    let mut stats = RuntimeGcStats::default();
    let Ok(entries) = fs::read_dir(root) else {
        return stats;
    };

    let mut system = System::new();
    let stale_after_ms = stale_after.as_millis() as u64;

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        stats.scanned_dirs += 1;

        let Some(runtime_entry) = read_runtime_dir_entry(&dir) else {
            debug!(path = %dir.display(), "runtime gc skipped directory without readable sidecar");
            continue;
        };

        let is_alive = runtime_entry
            .pid
            .is_some_and(|pid| process_matches(&mut system, pid, runtime_entry.spawned_at_unix_ms));
        if is_alive {
            match write_last_seen(&runtime_entry.sidecar_path, now_unix_ms) {
                Ok(true) => {
                    stats.refreshed_sidecars += 1;
                }
                Ok(false) => {}
                Err(err) => warn!(path = %runtime_entry.sidecar_path.display(), "{err}"),
            }
            continue;
        }

        let Some(last_seen_unix_ms) = runtime_entry.last_seen_unix_ms else {
            debug!(path = %runtime_entry.sidecar_path.display(), "runtime gc skipped sidecar without last_seen");
            continue;
        };
        if now_unix_ms.saturating_sub(last_seen_unix_ms) < stale_after_ms {
            continue;
        }

        match fs::remove_dir_all(&runtime_entry.dir) {
            Ok(()) => {
                stats.removed_dirs += 1;
                info!(path = %runtime_entry.dir.display(), "runtime gc removed stale daemon runtime dir");
            }
            Err(err) => {
                warn!(path = %runtime_entry.dir.display(), "runtime gc failed to remove directory: {err}");
            }
        }
    }

    stats
}

pub fn prune_runtime_root(root: &Path, stale_after: Duration) -> RuntimeGcStats {
    prune_runtime_root_at(root, stale_after, now_unix_ms())
}

pub async fn runtime_gc_loop(state: Arc<DaemonState>, interval_secs: u64, stale_after_secs: u64) {
    let mut shutdown_rx = state.shutdown_tx.subscribe();
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    let stale_after = Duration::from_secs(stale_after_secs);

    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let root = runtime_root();
                let result = tokio::task::spawn_blocking(move || prune_runtime_root(&root, stale_after)).await;
                match result {
                    Ok(stats) => {
                        if stats.removed_dirs > 0 || stats.refreshed_sidecars > 0 {
                            info!(
                                scanned = stats.scanned_dirs,
                                removed = stats.removed_dirs,
                                refreshed = stats.refreshed_sidecars,
                                "runtime gc scan completed"
                            );
                        } else {
                            debug!(scanned = stats.scanned_dirs, "runtime gc scan found nothing to remove");
                        }
                    }
                    Err(err) => warn!("runtime gc task panicked: {err}"),
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("runtime gc shutting down");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn create_runtime_dir(
        root: &Path,
        name: &str,
        pid: Option<u32>,
        sidecar_fields: Map<String, Value>,
    ) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        if let Some(pid) = pid {
            fs::write(dir.join("daemon.pid"), pid.to_string()).unwrap();
        }
        let sidecar_path = dir.join(format!("{name}.daemon.json"));
        fs::write(
            &sidecar_path,
            serde_json::to_string_pretty(&Value::Object(sidecar_fields)).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn prune_runtime_root_removes_dead_stale_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let mut sidecar = Map::new();
        sidecar.insert("command".into(), Value::from("python"));
        sidecar.insert("spawned_at_unix_ms".into(), Value::from(1_000_u64));
        sidecar.insert("last_seen_unix_ms".into(), Value::from(2_000_u64));
        let stale_dir = create_runtime_dir(temp.path(), "dead-stale", Some(4_000_000), sidecar);

        let stats = prune_runtime_root_at(temp.path(), Duration::from_secs(5), 10_000);

        assert_eq!(stats.scanned_dirs, 1);
        assert_eq!(stats.removed_dirs, 1);
        assert!(!stale_dir.exists());
    }

    #[test]
    fn prune_runtime_root_keeps_recent_dead_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let mut sidecar = Map::new();
        sidecar.insert("command".into(), Value::from("python"));
        sidecar.insert("spawned_at_unix_ms".into(), Value::from(1_000_u64));
        sidecar.insert("last_seen_unix_ms".into(), Value::from(9_500_u64));
        let recent_dir = create_runtime_dir(temp.path(), "dead-recent", Some(4_000_001), sidecar);

        let stats = prune_runtime_root_at(temp.path(), Duration::from_secs(5), 10_000);

        assert_eq!(stats.scanned_dirs, 1);
        assert_eq!(stats.removed_dirs, 0);
        assert!(recent_dir.exists());
    }

    #[test]
    fn prune_runtime_root_refreshes_alive_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let mut system = System::new();
        let my_pid = std::process::id();
        let sys_pid = Pid::from_u32(my_pid);
        system.refresh_process_specifics(sys_pid, ProcessRefreshKind::new());
        let spawned_at_unix_ms = system.process(sys_pid).unwrap().start_time() * 1000;

        let mut sidecar = Map::new();
        sidecar.insert("command".into(), Value::from("python"));
        sidecar.insert("spawned_at_unix_ms".into(), Value::from(spawned_at_unix_ms));
        sidecar.insert("last_seen_unix_ms".into(), Value::from(1_000_u64));
        let runtime_dir = create_runtime_dir(temp.path(), "alive-daemon", Some(my_pid), sidecar);

        let stats = prune_runtime_root_at(temp.path(), Duration::from_secs(5), 20_000);

        assert_eq!(stats.scanned_dirs, 1);
        assert_eq!(stats.removed_dirs, 0);
        assert_eq!(stats.refreshed_sidecars, 1);
        let sidecar_path = runtime_dir.join("alive-daemon.daemon.json");
        let data: Value = serde_json::from_str(&fs::read_to_string(sidecar_path).unwrap()).unwrap();
        assert_eq!(data["last_seen_unix_ms"].as_u64(), Some(20_000));
    }

    #[test]
    fn read_runtime_dir_entry_uses_spawned_at_as_last_seen_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let mut sidecar = Map::new();
        sidecar.insert("command".into(), Value::from("python"));
        sidecar.insert("spawned_at_unix_ms".into(), Value::from(42_000_u64));
        let runtime_dir = create_runtime_dir(temp.path(), "spawned-only", Some(1234), sidecar);

        let entry = read_runtime_dir_entry(&runtime_dir).expect("entry should parse");

        assert_eq!(entry.pid, Some(1234));
        assert_eq!(entry.spawned_at_unix_ms, Some(42_000));
        assert_eq!(entry.last_seen_unix_ms, Some(42_000));
        assert_eq!(
            entry.sidecar_path,
            runtime_dir.join("spawned-only.daemon.json")
        );
    }

    #[test]
    fn read_runtime_dir_entry_rejects_missing_or_invalid_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-sidecar");
        fs::create_dir_all(&missing).unwrap();
        assert!(read_runtime_dir_entry(&missing).is_none());

        let invalid = temp.path().join("invalid-sidecar");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("invalid-sidecar.daemon.json"), "{not json").unwrap();
        assert!(read_runtime_dir_entry(&invalid).is_none());

        let non_object = temp.path().join("non-object-sidecar");
        fs::create_dir_all(&non_object).unwrap();
        fs::write(non_object.join("non-object-sidecar.daemon.json"), "[]").unwrap();
        assert!(read_runtime_dir_entry(&non_object).is_none());
    }

    #[test]
    fn write_last_seen_updates_once_and_noops_for_same_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let sidecar_path = temp.path().join("runtime.daemon.json");
        fs::write(
            &sidecar_path,
            r#"{"command":"python","last_seen_unix_ms":1}"#,
        )
        .unwrap();

        assert!(write_last_seen(&sidecar_path, 10).expect("update should succeed"));
        let data: Value =
            serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(data["last_seen_unix_ms"].as_u64(), Some(10));

        assert!(!write_last_seen(&sidecar_path, 10).expect("same value should no-op"));
    }

    #[test]
    fn write_last_seen_rejects_non_object_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let sidecar_path = temp.path().join("runtime.daemon.json");
        fs::write(&sidecar_path, "[]").unwrap();

        let err = write_last_seen(&sidecar_path, 10).expect_err("array should fail");

        assert!(err.contains("is not a JSON object"));
    }

    #[test]
    fn process_matches_checks_pid_and_start_time() {
        let mut system = System::new();
        let pid = std::process::id();

        assert!(process_matches(&mut system, pid, None));
        assert!(!process_matches(&mut system, pid, Some(0)));
        assert!(!process_matches(&mut system, u32::MAX, None));
    }

    #[test]
    fn prune_runtime_root_skips_files_and_unusable_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("plain-file"), "not a runtime dir").unwrap();

        let missing_sidecar = temp.path().join("missing-sidecar");
        fs::create_dir_all(&missing_sidecar).unwrap();

        let mut no_timestamp = Map::new();
        no_timestamp.insert("command".into(), Value::from("python"));
        let no_timestamp_dir =
            create_runtime_dir(temp.path(), "no-timestamp", Some(4_000_002), no_timestamp);

        let stats = prune_runtime_root_at(temp.path(), Duration::from_secs(5), 10_000);

        assert_eq!(stats.scanned_dirs, 2);
        assert_eq!(stats.removed_dirs, 0);
        assert!(missing_sidecar.exists());
        assert!(no_timestamp_dir.exists());
    }
}
