//! soldr#2388: daemon **tombstone** — a short suppression window planted by an
//! explicit `soldr daemon stop`, during which **implicit** daemon
//! resurrections are cancelled. Post-Step-4 the one implicit-start path is the
//! broker's proactive daemon launch, so without this, stopping the daemon while
//! any `soldr` activity is happening would immediately trigger a thundering
//! herd of restarts — the exact failure this guards against.
//!
//! Contract:
//! * `soldr daemon stop` plants a tombstone expiring [`TOMBSTONE_DURATION`] from
//!   now.
//! * While it is live, [`is_active`] is true and the broker's proactive launch
//!   (and any other implicit start that consults it) skips spawning.
//! * `soldr daemon start` (explicit) [`clear`]s it and starts the daemon.
//! * It also auto-expires — a stale/expired tombstone is removed on read, so a
//!   forgotten stop never wedges the daemon permanently.
//!
//! The file is a single unix-millis expiry timestamp; no schema/versioning is
//! needed because it is transient local coordination state, not persisted or
//! transported metadata (so the protobuf mandate does not apply).

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::SoldrPaths;

/// How long an explicit stop suppresses implicit resurrections.
pub const TOMBSTONE_DURATION: Duration = Duration::from_secs(30);

fn tombstone_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("soldr-daemon").join("daemon-tombstone")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Plant a tombstone expiring `duration` from now (idempotent — overwrites any
/// existing one, extending the window).
pub fn plant(paths: &SoldrPaths, duration: Duration) {
    let expiry_ms = now_ms().saturating_add(duration.as_millis() as u64);
    let path = tombstone_path(paths);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, expiry_ms.to_string());
}

/// Clear any tombstone. An explicit `soldr daemon start` overrides the
/// suppression window.
pub fn clear(paths: &SoldrPaths) {
    let _ = std::fs::remove_file(tombstone_path(paths));
}

/// Whether a live (unexpired) tombstone is currently suppressing implicit
/// resurrections. An expired or corrupt tombstone is proactively removed and
/// reported inactive, so this never wedges the daemon after the window passes.
pub fn is_active(paths: &SoldrPaths) -> bool {
    let path = tombstone_path(paths);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    match text.trim().parse::<u64>() {
        Ok(expiry_ms) if now_ms() < expiry_ms => true,
        _ => {
            // Expired or corrupt: clean it up and report inactive.
            let _ = std::fs::remove_file(&path);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SoldrPaths;
    use std::time::Duration;

    fn temp_paths() -> (tempfile::TempDir, SoldrPaths) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(dir.path().join("root"));
        (dir, paths)
    }

    #[test]
    fn absent_tombstone_is_inactive() {
        let (_d, paths) = temp_paths();
        assert!(!is_active(&paths));
    }

    #[test]
    fn planted_tombstone_is_active_then_cleared() {
        let (_d, paths) = temp_paths();
        plant(&paths, Duration::from_secs(30));
        assert!(
            is_active(&paths),
            "a freshly planted tombstone must be active"
        );
        clear(&paths);
        assert!(!is_active(&paths), "clear must lift the suppression");
    }

    #[test]
    fn expired_tombstone_is_inactive_and_removed() {
        let (_d, paths) = temp_paths();
        // Already-expired window: `now < expiry` is false immediately.
        plant(&paths, Duration::from_millis(0));
        assert!(
            !is_active(&paths),
            "a zero/past-expiry tombstone must not suppress"
        );
        // And it is proactively removed, so it never wedges the daemon.
        assert!(!is_active(&paths));
    }
}
