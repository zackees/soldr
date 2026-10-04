//! Read claims from other generations for maintenance and diagnostics.
//!
//! Each generation owns its own `root-owner.lock` under
//! `generations/<service>/` (soldr#3566). A sibling route can coexist with
//! this one and cannot hold this route's lock.

use crate::core::SoldrPaths;
use std::path::PathBuf;

/// One daemon recorded under another generation's route claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationOwner {
    /// The generation directory name (the broker route service name), or
    /// `"legacy"` for the pre-#3374 version-independent slot.
    pub(crate) generation: String,
    pub(crate) pid: u32,
    pub(crate) exe: PathBuf,
}

/// Every claim recorded under `soldr-daemon/generations/*` plus the legacy
/// slot, in a stable order. Unreadable or malformed claims are skipped: this
/// only feeds a diagnostic and must never fail the caller.
pub(crate) fn recorded_generation_owners(paths: &SoldrPaths) -> Vec<GenerationOwner> {
    let daemon_dir = crate::cache_lib::soldr_daemon_dir(paths);
    let claim_file = super::backend_handle_adoption::BROKER_ROUTE_CLAIM_FILE;
    let mut owners = Vec::new();
    let mut push = |generation: String, path: PathBuf| {
        if let Ok(Some((pid, exe))) =
            super::backend_handle_adoption::read_claim_owner_identity_at(&path)
        {
            owners.push(GenerationOwner {
                generation,
                pid,
                exe,
            });
        }
    };
    push("legacy".to_string(), daemon_dir.join(claim_file));
    if let Ok(entries) = std::fs::read_dir(daemon_dir.join("generations")) {
        let mut dirs: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .collect();
        dirs.sort_by_key(|entry| entry.file_name());
        for entry in dirs {
            push(
                entry.file_name().to_string_lossy().into_owned(),
                entry.path().join(claim_file),
            );
        }
    }
    owners
}

/// The ownership-conflict text when this route has no recorded daemon.
/// Other generation claims are context only; they cannot identify this lock's
/// holder.
pub(crate) fn describe_unrecorded_owner(
    paths: &SoldrPaths,
    owners: &[GenerationOwner],
    is_alive: impl Fn(u32) -> bool,
) -> String {
    let root = paths.root.display();
    let lock = crate::daemon::generation_key::generation_state_dir(paths).join("root-owner.lock");
    let remedies = format!(
        "soldr: this route's lock is {}; identify its holder before terminating a process.\n\
         soldr: run `soldr status` and `soldr logs paths` to inspect this route.\n\
         soldr: use SOLDR_CACHE_DIR=<scratch dir> to build with a separate root.",
        lock.display()
    );
    if let Some(owner) = owners.iter().find(|owner| is_alive(owner.pid)) {
        return format!(
            "soldr root ownership is busy: {root} (no daemon route claim to name this route's lock holder).\n\
             soldr: sibling route generation {} has recorded PID {} (image {}); it can coexist and does not hold this route's lock.\n{remedies}",
            owner.generation,
            owner.pid,
            owner.exe.display(),
        );
    }
    format!(
        "soldr root ownership is busy: {root} (no daemon route claim to name this route's lock holder).\n{remedies}"
    )
}

#[cfg(test)]
#[path = "other_generations_tests.rs"]
mod tests;
