//! Per-generation keying of soldr-daemon's root-local state (soldr#3374).
//!
//! Split out of `backend_handle_adoption.rs` for the per-file LOC ceiling.

use crate::cache_lib::soldr_daemon_dir;
use crate::core::SoldrPaths;
use std::path::PathBuf;

/// Per-generation subdirectory name under `soldr-daemon/`, one per distinct
/// daemon image (broker route `service_name`, itself keyed by canonical
/// root + package version + image digest). soldr#3374: two soldr versions
/// used on one machine each get their own route-claim slot here, so a
/// preflight resolving `SOLDR_BROKER_SERVICE` to its own generation never
/// even sees the other generation's claim file, let alone treats it as
/// something to displace.
const GENERATIONS_SUBDIR: &str = "generations";

/// The generation key this process resolves to, when known.
///
/// `SOLDR_BROKER_SERVICE` first: the front door exports it before preflight
/// and the route registration pins it into the daemon's own environment.
/// Without it, the name is derived from this binary's daemon image (see
/// [`derive_generation_key`]); only when that fails too does the legacy
/// version-independent slot apply. A new generation never adopts that slot
/// as its own, so an older daemon living there is neither overwritten nor
/// seen as this generation's daemon.
fn resolved_generation_key() -> Option<String> {
    if let Some(key) = GENERATION_OVERRIDE.with(|key| key.borrow().clone()) {
        return Some(key).filter(|value| !value.is_empty());
    }
    // Unit tests never fall through to the process environment, which every
    // other claim test in this binary reads concurrently.
    #[cfg(test)]
    let key: Option<String> = None;
    #[cfg(not(test))]
    let key = std::env::var(super::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(derive_generation_key);
    key.filter(|value| !value.is_empty())
}

/// Derive this binary's own generation when `SOLDR_BROKER_SERVICE` is not
/// exported (a plain `soldr status` / `soldr daemon stop`). Guarded against
/// re-entry: deriving the service name may consult the route claim, which
/// resolves its path through here; the inner lookup takes the legacy slot.
#[cfg(not(test))]
fn derive_generation_key() -> Option<String> {
    thread_local! {
        static DERIVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DERIVING.with(|flag| flag.replace(true)) {
        return None;
    }
    let key = super::backend_handle_adoption::broker_service_name().ok();
    DERIVING.with(|flag| flag.set(false));
    key
}

thread_local! {
    static GENERATION_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with route-claim state keyed to `service_name` on this thread.
///
/// The broker serves every route from one process, so its launcher cannot
/// use the process-wide `SOLDR_BROKER_SERVICE`; it names the route of the
/// request it is handling instead. Tests use the same seam.
#[cfg(test)]
pub(crate) fn set_generation_override(key: Option<String>) {
    GENERATION_OVERRIDE.with(|slot| *slot.borrow_mut() = key);
}

pub fn with_generation_key<R>(service_name: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            GENERATION_OVERRIDE.with(|key| *key.borrow_mut() = previous);
        }
    }
    let previous =
        GENERATION_OVERRIDE.with(|key| key.borrow_mut().replace(service_name.to_string()));
    let _restore = Restore(previous);
    f()
}

pub(crate) fn generation_state_dir(paths: &SoldrPaths) -> PathBuf {
    let dir = soldr_daemon_dir(paths);
    match resolved_generation_key() {
        Some(key) => dir.join(GENERATIONS_SUBDIR).join(key),
        None => dir,
    }
}
