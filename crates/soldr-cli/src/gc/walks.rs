//! Filesystem walks for the gc taxonomy.
//!
//! Owns the `cargo_registry_src` and `cargo_git_checkouts` walkers
//! plus the shared `last-used` resolution rule that the registry-src
//! walker uses to combine cargo's `.global-cache` SQLite tracker with
//! the filesystem mtime fallback (#349). Also hosts the cheap
//! parallel directory sizer that several gc surfaces share.

use super::{
    GcListEntryOutput, KIND_CARGO_GIT_CHECKOUTS, KIND_CARGO_REGISTRY_SRC, PURGE_SAFETY_DERIVED,
};

/// Provenance tag for `last_used_unix` (#349).
pub(super) const LAST_USED_FROM_GLOBAL_CACHE: &str = "global_cache";
pub(super) const LAST_USED_FROM_FS_MTIME: &str = "fs_mtime";

pub(super) fn absolute_path_string(path: &std::path::Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Compute `(size_bytes, file_count)` for a directory using rayon to
/// fan out across the top-level entries. The per-entry walk is the
/// existing sequential routine. This keeps the implementation small
/// while exploiting the typical cargo `target/` layout where the bulk
/// of bytes sit under a handful of subdirs (`debug/`, `release/`,
/// per-target triples, etc.).
pub(super) fn fast_directory_size_and_files(path: &std::path::Path) -> (u64, u64) {
    use rayon::prelude::*;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if metadata.file_type().is_symlink() {
        return (0, 0);
    }
    if metadata.is_file() {
        return (metadata.len(), 1);
    }
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(path) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => return (0, 0),
    };
    entries
        .into_par_iter()
        .map(|entry| {
            let entry_path = entry.path();
            let entry_meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return (0u64, 0u64),
            };
            if entry_meta.file_type().is_symlink() {
                (0, 0)
            } else if entry_meta.is_dir() {
                crate::cache_lib::target_registry::directory_size_and_files(&entry_path)
            } else if entry_meta.is_file() {
                (entry_meta.len(), 1)
            } else {
                (0, 0)
            }
        })
        .reduce(
            || (0u64, 0u64),
            |a, b| (a.0.saturating_add(b.0), a.1.saturating_add(b.1)),
        )
}

// ---------------------------------------------------------------------------
// cargo_registry_src walker (#323 slice 2).
// ---------------------------------------------------------------------------

/// Walk `$CARGO_HOME/registry/src/<registry-hash-dir>/<crate>-<vers>/`
/// and produce a `GcListEntryOutput` per crate directory.
///
/// `last_used_unix` is preferentially derived from cargo's own
/// `$CARGO_HOME/.global-cache` SQLite tracker (#349). When the
/// tracker is missing, locked, schema-drifted, or has no row for a
/// particular crate, we fall back to the directory's filesystem
/// mtime. The `last_used_source` field on each entry records which
/// provenance produced the timestamp so JSON consumers can gate
/// metrics on it.
pub(super) fn walk_cargo_registry_src(
    cargo_home: &std::path::Path,
    now: i64,
) -> Vec<GcListEntryOutput> {
    let registry_src = cargo_home.join("registry").join("src");
    let registry_dirs = match std::fs::read_dir(&registry_src) {
        Ok(iter) => iter,
        Err(_) => return Vec::new(),
    };

    // Try the global-cache tracker once up-front. None covers the
    // "missing / locked / schema-drift" cases; every crate then falls
    // back to mtime. An empty Some(..) means the tracker exists but
    // has no rows; each crate that misses the lookup still falls
    // back individually.
    let global_cache =
        crate::cache_lib::cargo_global_cache::read_registry_src_last_used(cargo_home);

    let mut out: Vec<GcListEntryOutput> = Vec::new();
    for reg_entry in registry_dirs.flatten() {
        let reg_path = reg_entry.path();
        let Ok(reg_meta) = reg_entry.metadata() else {
            continue;
        };
        if !reg_meta.is_dir() {
            continue;
        }
        let registry_hash = match reg_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let crate_dirs = match std::fs::read_dir(&reg_path) {
            Ok(iter) => iter,
            Err(_) => continue,
        };
        for crate_entry in crate_dirs.flatten() {
            let crate_path = crate_entry.path();
            let Ok(meta) = crate_entry.metadata() else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let dir_name = match crate_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let owner_crate = parse_crate_owner(&dir_name);
            let (size_bytes, file_count) = fast_directory_size_and_files(&crate_path);
            let (last_used_unix, last_used_source) = resolve_registry_src_last_used(
                global_cache.as_ref(),
                &registry_hash,
                &dir_name,
                &meta,
            );
            let age_seconds = now.saturating_sub(last_used_unix);
            out.push(GcListEntryOutput {
                path: absolute_path_string(&crate_path),
                last_used_unix,
                age_seconds,
                age_human: crate::cache_lib::target_registry::human_age(age_seconds),
                size_bytes,
                size_human: crate::cache_lib::target_registry::human_size(size_bytes),
                file_count,
                kind: KIND_CARGO_REGISTRY_SRC,
                purge_safety: PURGE_SAFETY_DERIVED,
                owner_crate,
                last_used_source: Some(last_used_source),
            });
        }
    }
    out
}

/// Pick the `last_used_unix` value (and its provenance tag) for one
/// crate source directory. Pulled out so the precedence rule can be
/// unit-tested without a real cargo install on disk.
///
/// Returns `(unix_seconds, "global_cache" | "fs_mtime")`.
pub(super) fn resolve_registry_src_last_used(
    global_cache: Option<
        &std::collections::HashMap<crate::cache_lib::cargo_global_cache::RegistrySrcKey, i64>,
    >,
    registry_hash: &str,
    dir_name: &str,
    meta: &std::fs::Metadata,
) -> (i64, &'static str) {
    if let (Some(map), Some((crate_name, version))) = (global_cache, split_dir_name(dir_name)) {
        let key = (
            registry_hash.to_string(),
            crate_name.to_string(),
            version.to_string(),
        );
        if let Some(&ts) = map.get(&key) {
            return (ts, LAST_USED_FROM_GLOBAL_CACHE);
        }
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (mtime, LAST_USED_FROM_FS_MTIME)
}

/// Split `<crate>-<version>` directory names into `(crate, version)`
/// using the same rule as [`parse_crate_owner`]: the last hyphen
/// followed by an ASCII digit is the boundary. Returns `None` for
/// names that don't match the shape (e.g. a bare `serde/` dir).
pub(super) fn split_dir_name(dir_name: &str) -> Option<(&str, &str)> {
    let bytes = dir_name.as_bytes();
    for (idx, &b) in bytes.iter().enumerate().rev() {
        if b == b'-' && idx + 1 < bytes.len() && bytes[idx + 1].is_ascii_digit() {
            let (name, rest) = dir_name.split_at(idx);
            let version = &rest[1..];
            if name.is_empty() {
                return None;
            }
            return Some((name, version));
        }
    }
    None
}

/// Parse `<crate>-<vers>` directory names into `Some("<crate>@<vers>")`.
///
/// Algorithm: find the **last** `'-'` whose suffix starts with an ASCII
/// digit. That suffix is the semver; everything before is the crate
/// name. Returns `None` if the input has no such hyphen (e.g. a bare
/// `serde/` dir from an aberrant layout).
pub(super) fn parse_crate_owner(dir_name: &str) -> Option<String> {
    let bytes = dir_name.as_bytes();
    for (idx, &b) in bytes.iter().enumerate().rev() {
        if b == b'-' && idx + 1 < bytes.len() && bytes[idx + 1].is_ascii_digit() {
            let (name, rest) = dir_name.split_at(idx);
            // rest includes the leading '-'; strip it.
            let version = &rest[1..];
            if name.is_empty() {
                return None;
            }
            return Some(format!("{name}@{version}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// cargo_git_checkouts walker (#323 slice 3).
// ---------------------------------------------------------------------------

/// Walk `$CARGO_HOME/git/checkouts/<repo>/<commit>/` and produce a
/// `GcListEntryOutput` per checkout directory.
///
/// Layout reminder: cargo stores git-source crates as a bare clone at
/// `~/.cargo/git/db/<repo>/` (the **primary** copy — never pruned by
/// soldr because cargo owns it) and a per-commit worktree at
/// `~/.cargo/git/checkouts/<repo>/<commit>/` (this is what we surface).
/// The worktree is fully regeneratable from the bare clone, so safety
/// class is `derived`.
///
/// `last_used_unix` is the directory's filesystem mtime today. Cargo's
/// `$CARGO_HOME/.global-cache` SQLite tracker also records git-checkout
/// touch events; integrating that lookup is straightforward but lives in
/// a follow-up (mirrors the registry-src precedence introduced in #349).
/// Until then the mtime fallback gives a usable approximation.
pub(super) fn walk_cargo_git_checkouts(
    cargo_home: &std::path::Path,
    now: i64,
) -> Vec<GcListEntryOutput> {
    let checkouts_root = cargo_home.join("git").join("checkouts");
    let repo_dirs = match std::fs::read_dir(&checkouts_root) {
        Ok(iter) => iter,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<GcListEntryOutput> = Vec::new();
    for repo_entry in repo_dirs.flatten() {
        let repo_path = repo_entry.path();
        let Ok(repo_meta) = repo_entry.metadata() else {
            continue;
        };
        if !repo_meta.is_dir() {
            continue;
        }
        let repo_dir_name = match repo_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let commit_dirs = match std::fs::read_dir(&repo_path) {
            Ok(iter) => iter,
            Err(_) => continue,
        };
        for commit_entry in commit_dirs.flatten() {
            let commit_path = commit_entry.path();
            let Ok(commit_meta) = commit_entry.metadata() else {
                continue;
            };
            if !commit_meta.is_dir() {
                continue;
            }
            let commit_dir_name = match commit_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let (size_bytes, file_count) = fast_directory_size_and_files(&commit_path);
            let last_used_unix = commit_meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let age_seconds = now.saturating_sub(last_used_unix);
            out.push(GcListEntryOutput {
                path: absolute_path_string(&commit_path),
                last_used_unix,
                age_seconds,
                age_human: crate::cache_lib::target_registry::human_age(age_seconds),
                size_bytes,
                size_human: crate::cache_lib::target_registry::human_size(size_bytes),
                file_count,
                kind: KIND_CARGO_GIT_CHECKOUTS,
                purge_safety: PURGE_SAFETY_DERIVED,
                owner_crate: Some(format!("{repo_dir_name}@{commit_dir_name}")),
                last_used_source: Some(LAST_USED_FROM_FS_MTIME),
            });
        }
    }
    out
}
