//! Linked zccache lifecycle. When the soldr-daemon's session has
//! registered a zccache runtime/cache/session via `LinkZccache`, the
//! soldr-daemon's own shutdown runs `zccache stop` against that exact
//! cache namespace before exiting.

use crate::core::SoldrPaths;
use crate::daemon::db;
use crate::zccache_lifecycle::ZccacheLifecycle;
use std::time::Duration;

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Best-effort: if a linked zccache runtime is recorded, spawn
/// `zccache stop` with its recorded cache dir and wait up to 5 seconds.
/// All errors are swallowed so a hung zccache cannot keep soldr-daemon alive.
pub fn stop_linked_zccache(paths: &SoldrPaths) {
    let db_path = db::db_path(paths);
    let Ok(Some(link)) = db::get_linked_zccache(&db_path) else {
        return;
    };

    let binary_path = std::path::PathBuf::from(&link.binary_path);
    let cache_dir = std::path::PathBuf::from(&link.cache_dir);
    if !binary_path.exists() {
        let _ = db::set_linked_zccache(&db_path, None);
        return;
    }

    let mut lifecycle = ZccacheLifecycle::new(binary_path, cache_dir);
    let _ = lifecycle.stop_best_effort_with_process_timeout(STOP_TIMEOUT);
    let _ = db::set_linked_zccache(&db_path, None);
}
