//! soldr#2360: actionable attribution for a daemon-unavailable compile
//! dispatch failure.
//!
//! Before this module existed, `DispatchError::into_soldr_error()` just
//! collapsed any terminal failure into `SoldrError::Other(self.to_string())`.
//! For a healthy daemon that answered with its own error, that is correct —
//! but for daemon *unavailability* (`NotRunning` / `Io` / `Retiring` /
//! `CompileStalled`), the resulting exit code 1 surfaces to cargo as a bare
//! `error: could not compile <crate>`, indistinguishable from a real
//! compiler error. That was the reported symptom of #2360, reproduced
//! against the #2352 orphan-lock wedge: every crate that hit the down
//! daemon failed with nothing naming soldr as the cause.
//!
//! `should_fall_back_to_direct_rustc` still returns `false`
//! unconditionally (soldr#2256's no-silent-fallback intent is unchanged);
//! this module only changes what the resulting hard failure *says*, never
//! whether it fails.

use crate::compile_dispatch::DispatchError;
use crate::core::{SoldrError, SoldrPaths};

/// Actionable infra-attributed message for a compile that could not be served
/// because the broker (and therefore the broker-launched daemon) was
/// unavailable. soldr#2388: all compiles route through the broker and there is
/// no direct-daemon fallback, so this names the concrete remedy rather than
/// degrading silently. Lives here (not in the over-ceiling `compile_dispatch`)
/// alongside the other daemon-unavailable remedy text.
pub(crate) fn broker_unavailable_remedy() -> String {
    "soldr: compile could not be served — the broker-fronted compile daemon was \
     unavailable (not a compiler error). All compiles route through the broker; \
     there is no direct-daemon fallback. Remedy: run a top-level `soldr cargo …` \
     build (its front door starts the broker), or start one explicitly with \
     `soldr broker serve`; then re-run. See `soldr status` / `soldr doctor` and \
     docs/DAEMON_TIMEOUTS.md."
        .to_string()
}

/// See the module docs. Free function (rather than living directly on
/// [`DispatchError`]) so the substantial remedy-lookup logic stays out of
/// `compile_dispatch.rs`, which is already over the repo's per-file line
/// ceiling (soldr#1966) and may not grow.
pub fn into_soldr_error(err: DispatchError) -> SoldrError {
    if !err.is_daemon_unavailable() {
        return SoldrError::Other(err.to_string());
    }
    let remedy = SoldrPaths::new()
        .ok()
        .and_then(|paths| daemon_unavailable_remedy(&paths))
        .filter(|s| !s.is_empty())
        .map(|s| format!("\nsoldr: {s}"))
        .unwrap_or_default();
    SoldrError::Other(format!(
        "soldr: compile daemon infrastructure failure (not a compiler error): {err}{remedy}"
    ))
}

/// Explain *why* the daemon was unavailable, if we can tell.
///
/// The root-ownership lock is checked live (acquire-then-immediately-drop,
/// the same peek pattern `soldr-daemon`'s `maintenance.rs` already uses)
/// rather than assumed. A daemon-unavailable failure with no lock
/// contention — e.g. a daemon that is still cold-starting — must not get
/// a misleading "root ownership is busy" diagnosis; it gets pointed at the
/// general diagnostics instead.
fn daemon_unavailable_remedy(paths: &SoldrPaths) -> Option<String> {
    match crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths) {
        // Lock is free: root ownership is not the cause.
        Ok(Some(_guard)) => Some(
            "run `soldr status` / `soldr doctor` for daemon diagnostics; \
             see docs/DAEMON_TIMEOUTS.md for the full runbook."
                .to_string(),
        ),
        // Contested: this is the #2352 orphan-lock wedge.
        Ok(None) => Some(crate::daemon::lifecycle::describe_root_ownership_conflict(
            paths,
        )),
        // Couldn't even check — say nothing rather than guess.
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_dispatch::DispatchError;
    use crate::daemon::client::ClientError;
    use crate::timed_test;
    use std::path::PathBuf;
    use std::time::Duration;

    fn budget_exhausted_with(last_err: Option<ClientError>) -> DispatchError {
        DispatchError::BudgetExhausted {
            budget: Duration::from_millis(250),
            last_err,
            sock: PathBuf::from("/tmp/soldr-test-sock"),
            spawn_err: None,
        }
    }

    // Serializes tests that mutate `SOLDR_CACHE_DIR` — other test binaries
    // in this crate use the same pattern (`compile_dispatch::tests::ENV_MUTEX`)
    // but that guard is private to its module, so this module keeps its own.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // soldr#2360: a daemon-unavailable failure must be attributed to soldr
    // infrastructure, never read as a bare compiler error — and must never
    // falsely claim orphan-lock contention that isn't actually there.
    timed_test!(
        into_soldr_error_labels_infrastructure_failure_without_claiming_lock_contention,
        Duration::from_secs(5),
        {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(crate::core::SOLDR_CACHE_DIR_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            std::env::set_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR, temp.path());

            let msg =
                into_soldr_error(budget_exhausted_with(Some(ClientError::NotRunning))).to_string();

            match prior {
                Some(v) => std::env::set_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR, v),
                None => std::env::remove_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR),
            }

            assert!(
                msg.contains("infrastructure failure (not a compiler error)"),
                "must attribute the failure to soldr, not the compiler: {msg}"
            );
            assert!(
                !msg.contains("root ownership is busy"),
                "the root lock is uncontested here; must not claim otherwise: {msg}"
            );
            assert!(
                msg.contains("soldr status") || msg.contains("soldr doctor"),
                "uncontested case still needs an actionable next step: {msg}"
            );
        }
    );

    timed_test!(
        into_soldr_error_surfaces_orphan_lock_remedy_when_root_lock_contested,
        Duration::from_secs(5),
        {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(crate::core::SOLDR_CACHE_DIR_ENV_VAR);
            let temp = tempfile::tempdir().expect("tempdir");
            std::env::set_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR, temp.path());

            let paths = SoldrPaths::new().expect("resolve test paths");
            let holder = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
                .expect("lock io")
                .expect("lock is free at test start");

            let msg =
                into_soldr_error(budget_exhausted_with(Some(ClientError::NotRunning))).to_string();
            drop(holder);

            match prior {
                Some(v) => std::env::set_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR, v),
                None => std::env::remove_var(crate::core::SOLDR_CACHE_DIR_ENV_VAR),
            }

            assert!(
                msg.contains("infrastructure failure (not a compiler error)"),
                "{msg}"
            );
            assert!(
                msg.contains("root ownership is busy"),
                "the lock IS contested here; the remedy must say so: {msg}"
            );
        }
    );

    timed_test!(
        into_soldr_error_never_attaches_daemon_remedy_to_a_healthy_daemon_error,
        Duration::from_secs(5),
        {
            // A `Protocol` failure is a responding daemon, not
            // unavailability — `is_daemon_unavailable()` is false, so this
            // must fall straight through to the pre-#1300 plain format with
            // no infrastructure header and no remedy text attached.
            let msg = into_soldr_error(budget_exhausted_with(Some(ClientError::Protocol(
                "daemon-side error".into(),
            ))))
            .to_string();
            assert!(
                !msg.contains("infrastructure failure (not a compiler error)"),
                "a healthy daemon's own error must not be relabeled as infra: {msg}"
            );
        }
    );
}
