//! GC fixtures shared by the `cli_gc*` integration tests.
//!
//! Split out of `common/mod.rs` for soldr#1966: that file is over the 1,500
//! line ceiling, so the ratchet forbids it growing, and soldr#2134 needed
//! these two helpers to say a little more. Moving them shrinks `mod.rs`,
//! which the ratchet does allow -- and this is the split it exists to ask
//! for.

use super::toml_string;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn seed_gc_candidate(cache_root: &Path, label: &str) -> PathBuf {
    let dev_root = cache_root.join("dev-root");
    let workspace = dev_root.join(label);
    let target = workspace.join("target");
    fs::create_dir_all(&target).expect("failed to create target dir");
    // Cargo always writes this, and since #1671 the deletion path refuses to
    // recursively remove a directory that is merely *named* `target` without a
    // cargo marker. Without it this fixture is indistinguishable from an
    // arbitrary directory, so GC correctly declines to reclaim it and every
    // test asserting reclamation fails.
    fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172")
        .expect("failed to seed cargo target marker");
    fs::write(target.join("artifact.bin"), b"reclaim me").expect("failed to seed target file");
    fs::write(
        cache_root.join("config.toml"),
        format!("[gc]\nallowlist_roots = [\"{}\"]\n", toml_string(&dev_root)),
    )
    .expect("failed to write gc config");

    let registry = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
        &cache_root.join("state.sqlite3"),
    )
    .expect("failed to open target registry");
    let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
        .expect("failed to get current unix seconds");
    registry
        .upsert_with_time(&target, now - 120)
        .expect("failed to seed target registry");
    // soldr#2134: GC now ages a target by the more recent of its registry
    // stamp and its directory mtime, so a stale stamp on something created
    // milliseconds ago no longer reads as cold -- which is the point of that
    // change. A fixture meaning "this is reclaimable" has to say so on both
    // signals; before, it could rely on the mtime being ignored.
    let cold = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(cold))
        .expect("failed to backdate target mtime");
    target
}

/// [`seed_gc_candidate`] whose workspace root looks like a **linked git
/// worktree**: `.git` as a file containing a `gitdir:` pointer, which is
/// the signal `cache_lib::gc::in_linked_git_worktree` reads (soldr#2134).
///
/// A primary checkout holds `.git` as a *directory*, so the plain fixture
/// above is correctly not a worktree and the two can be told apart.
pub(crate) fn seed_gc_worktree_candidate(cache_root: &Path, label: &str) -> PathBuf {
    let target = seed_gc_candidate(cache_root, label);
    let workspace = target.parent().expect("target has a workspace parent");
    fs::write(
        workspace.join(".git"),
        format!(
            "gitdir: /somewhere/.git/worktrees/{label}
"
        ),
    )
    .expect("failed to seed linked-worktree marker");
    target
}

pub(crate) fn seed_gc_file_candidate(cache_root: &Path, label: &str) -> PathBuf {
    let dev_root = cache_root.join("dev-root");
    let workspace = dev_root.join(label);
    fs::create_dir_all(&workspace).expect("failed to create workspace dir");
    let target = workspace.join("target");
    fs::write(&target, b"not a directory").expect("failed to seed target file");
    fs::write(
        cache_root.join("config.toml"),
        format!("[gc]\nallowlist_roots = [\"{}\"]\n", toml_string(&dev_root)),
    )
    .expect("failed to write gc config");

    let registry = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
        &cache_root.join("state.sqlite3"),
    )
    .expect("failed to open target registry");
    let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
        .expect("failed to get current unix seconds");
    registry
        .upsert_with_time(&target, now - 120)
        .expect("failed to seed target registry");
    // soldr#2134: GC now ages a target by the more recent of its registry
    // stamp and its directory mtime, so a stale stamp on something created
    // milliseconds ago no longer reads as cold -- which is the point of that
    // change. A fixture meaning "this is reclaimable" has to say so on both
    // signals; before, it could rely on the mtime being ignored.
    let cold = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(cold))
        .expect("failed to backdate target mtime");
    target
}
