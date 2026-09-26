//! Read a fixture daemon's route claim from the test process (soldr#3374).
//!
//! soldr#3374 keys route claims per daemon generation, under
//! `<root>/soldr-daemon/generations/<service name>/`. A test process has no
//! `SOLDR_BROKER_SERVICE`, and deriving the key from `current_exe()` names the
//! *test binary's* generation, so a plain `read_broker_route_claim` looks in a
//! slot nothing ever wrote and reports no daemon. A fixture root is fresh, so
//! the generations it holds are exactly the ones its daemons published: try
//! each of them under its own key.

use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::backend_handle_adoption::{read_broker_route_claim, with_generation_key};
use std::path::Path;

/// Generation keys that have published state under `root`, sorted for a
/// deterministic scan.
fn published_generations(root: &Path) -> Vec<String> {
    let paths = SoldrPaths::with_root(root.to_path_buf());
    let dir = soldr_cli::cache_lib::soldr_daemon_dir(&paths).join("generations");
    let mut keys: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    keys.sort();
    keys
}

/// Run `f` under each published generation's key (then the legacy unkeyed
/// slot) and return the first `Some`.
pub(crate) fn with_published_generation<R>(root: &Path, f: impl Fn() -> Option<R>) -> Option<R> {
    published_generations(root)
        .iter()
        .find_map(|key| with_generation_key(key, &f))
        .or_else(&f)
}

/// PID recorded in the route claim of the daemon serving `root`.
pub(crate) fn route_claim_pid(root: &Path) -> Option<u32> {
    let paths = SoldrPaths::with_root(root.to_path_buf());
    with_published_generation(root, || {
        read_broker_route_claim(&paths)
            .ok()
            .flatten()
            .map(|claim| claim.pid)
    })
}
