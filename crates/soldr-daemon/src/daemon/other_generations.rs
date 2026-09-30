//! Naming a root's owner when it belongs to a different daemon generation
//! (soldr#3456).
//!
//! `root-owner.lock` is one lock per root, held for a daemon's whole life,
//! while route claims are keyed per generation (`generations/<service>/`,
//! soldr#3374). A daemon of another Soldr version can therefore hold the lock
//! while this version's own claim slot is empty, and the ownership error used
//! to say "no daemon route claim to name the owner" -- true only of *this*
//! generation's slot. This module reads the other generations' claims so the
//! error can name the process that actually holds the root.

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

/// The ownership-conflict text for a root whose own generation recorded no
/// daemon. Names a live process from another generation when one is recorded;
/// otherwise hedges, because nothing on disk identifies the holder.
pub(crate) fn describe_unrecorded_owner(
    root: &std::path::Display<'_>,
    owners: &[GenerationOwner],
    is_alive: impl Fn(u32) -> bool,
) -> String {
    let remedies = "\
         soldr: only one daemon can own a root at a time, so a different Soldr version or image cannot start a second one.\n\
         soldr: to build now without touching it, use an isolated root: SOLDR_CACHE_DIR=<scratch dir>.\n\
         soldr: to retire it deliberately, stop it from the Soldr that started it (`soldr daemon stop`) or run `soldr broker remove`; nothing is replaced automatically.";
    if let Some(owner) = owners.iter().find(|owner| is_alive(owner.pid)) {
        return format!(
            "soldr root ownership is busy: {root} (held by PID {}, image {}, route generation {} -- \
             a different Soldr version or daemon image than this one)\n{remedies}",
            owner.pid,
            owner.exe.display(),
            owner.generation
        );
    }
    format!(
        "soldr root ownership is busy: {root} (no daemon route claim to name the owner)\n\
         soldr: another Soldr, possibly a different version, is likely using this root.\n\
         soldr: run `soldr status` and `soldr logs paths` to see the running broker and daemon.\n{remedies}"
    )
}

#[cfg(test)]
#[path = "other_generations_tests.rs"]
mod tests;
