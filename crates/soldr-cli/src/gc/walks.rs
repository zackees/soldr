//! Filesystem walks for the gc taxonomy.
//!
//! Owns the `cargo_registry_src` and `cargo_git_checkouts` walkers
//! plus the shared `last-used` resolution rule both walkers use to
//! combine cargo's `.global-cache` SQLite tracker with the filesystem
//! mtime fallback (#349 for registry-src, #1544 for git checkouts).
//! Also hosts the cheap parallel directory sizer that several gc
//! surfaces share.

use super::{
    GcListEntryOutput, GcListKindFilter, KIND_CARGO_GIT_CHECKOUTS, KIND_CARGO_GIT_DB,
    KIND_CARGO_INSTALLED_BINARIES, KIND_CARGO_REGISTRY_CACHE, KIND_CARGO_REGISTRY_SRC,
    KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES, KIND_CARGO_TARGET_DOC, KIND_CARGO_TARGET_INCREMENTAL,
    KIND_CARGO_TARGET_SUBCOMMAND_CACHES, KIND_RUSTUP_TOOLCHAIN, PURGE_SAFETY_DERIVED,
    PURGE_SAFETY_PRIMARY,
};

const SUBCOMMAND_CACHE_DIRS: &[&str] = &[
    "criterion",
    "llvm-cov",
    "coverage",
    "nextest",
    "tarpaulin",
    "wasm-bindgen",
];

#[derive(Default)]
struct EntryOwners {
    owner_workspace: Option<String>,
    owner_crate: Option<String>,
    owner_repo: Option<String>,
    owner_binary: Option<String>,
    owner_toolchain: Option<String>,
}

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
                owner_workspace: None,
                owner_repo: None,
                owner_binary: None,
                owner_toolchain: None,
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
/// `last_used_unix` is preferentially derived from cargo's own
/// `$CARGO_HOME/.global-cache` SQLite tracker (#1544, mirroring the
/// registry-src precedence from #349). Cargo sets the checkout
/// directory's mtime once at checkout time and never touches it again
/// — only the tracker row is refreshed on later builds — so an
/// actively-used checkout looks arbitrarily old to an mtime-only
/// walker and gets purge-selected, forcing a purge/re-checkout loop.
/// When the tracker is missing, locked, schema-drifted, or has no row
/// for a particular checkout, we fall back to the directory mtime.
pub(super) fn walk_cargo_git_checkouts(
    cargo_home: &std::path::Path,
    now: i64,
) -> Vec<GcListEntryOutput> {
    let checkouts_root = cargo_home.join("git").join("checkouts");
    let repo_dirs = match std::fs::read_dir(&checkouts_root) {
        Ok(iter) => iter,
        Err(_) => return Vec::new(),
    };

    // Try the global-cache tracker once up-front. None covers the
    // "missing / locked / schema-drift" cases; every checkout then
    // falls back to mtime. An empty Some(..) means the tracker exists
    // but has no rows; each checkout that misses the lookup still
    // falls back individually.
    let global_cache =
        crate::cache_lib::cargo_global_cache::read_git_checkout_last_used(cargo_home);

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
            let (last_used_unix, last_used_source) = resolve_git_checkout_last_used(
                global_cache.as_ref(),
                &repo_dir_name,
                &commit_dir_name,
                &commit_meta,
            );
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
                owner_workspace: None,
                owner_repo: Some(repo_dir_name.clone()),
                owner_binary: None,
                owner_toolchain: None,
                last_used_source: Some(last_used_source),
            });
        }
    }
    out
}

/// Pick the `last_used_unix` value (and its provenance tag) for one
/// git checkout directory (#1544). Mirrors
/// [`resolve_registry_src_last_used`]: prefer cargo's `.global-cache`
/// row keyed by `(git-db-dirname, checkout-dirname)`, fall back to
/// the directory's filesystem mtime per-checkout.
///
/// Returns `(unix_seconds, "global_cache" | "fs_mtime")`.
pub(super) fn resolve_git_checkout_last_used(
    global_cache: Option<
        &std::collections::HashMap<crate::cache_lib::cargo_global_cache::GitCheckoutKey, i64>,
    >,
    repo_dir_name: &str,
    commit_dir_name: &str,
    meta: &std::fs::Metadata,
) -> (i64, &'static str) {
    if let Some(map) = global_cache {
        let key = (repo_dir_name.to_string(), commit_dir_name.to_string());
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

// ---------------------------------------------------------------------------
// Tracked target subtree walkers (#323 slice 4).
// ---------------------------------------------------------------------------

pub(super) fn walk_cargo_target_subtrees(
    rows: &[crate::cache_lib::target_registry::TargetRow],
    now: i64,
    kind_filter: Option<GcListKindFilter>,
) -> Vec<GcListEntryOutput> {
    if matches!(kind_filter, Some(kind) if !kind.is_target_subtree()) {
        return Vec::new();
    }

    let mut out = Vec::new();
    for row in rows {
        let owner_workspace =
            crate::cache_lib::target_registry::workspace_root_for_target(&row.path)
                .display()
                .to_string();

        if kind_filter.is_none()
            || matches!(kind_filter, Some(GcListKindFilter::CargoTargetIncremental))
        {
            for profile in profile_dirs(&row.path) {
                let path = profile.join("incremental");
                if let Some(entry) = entry_output_for_path(
                    &path,
                    KIND_CARGO_TARGET_INCREMENTAL,
                    PURGE_SAFETY_DERIVED,
                    now,
                    EntryOwners {
                        owner_workspace: Some(owner_workspace.clone()),
                        ..EntryOwners::default()
                    },
                ) {
                    out.push(entry);
                }
            }
        }

        if kind_filter.is_none()
            || matches!(
                kind_filter,
                Some(GcListKindFilter::CargoTargetBuildScriptBinaries)
            )
        {
            for profile in profile_dirs(&row.path) {
                let build_dir = profile.join("build");
                let read = match std::fs::read_dir(&build_dir) {
                    Ok(it) => it,
                    Err(_) => continue,
                };
                for unit in read.flatten() {
                    if !unit.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let unit_read = match std::fs::read_dir(unit.path()) {
                        Ok(it) => it,
                        Err(_) => continue,
                    };
                    for child in unit_read.flatten() {
                        let path = child.path();
                        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        if !is_build_script_binary(name) {
                            continue;
                        }
                        if let Some(entry) = entry_output_for_path(
                            &path,
                            KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES,
                            PURGE_SAFETY_DERIVED,
                            now,
                            EntryOwners {
                                owner_workspace: Some(owner_workspace.clone()),
                                ..EntryOwners::default()
                            },
                        ) {
                            out.push(entry);
                        }
                    }
                }
            }
        }

        if kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoTargetDoc)) {
            let path = row.path.join("doc");
            if let Some(entry) = entry_output_for_path(
                &path,
                KIND_CARGO_TARGET_DOC,
                PURGE_SAFETY_DERIVED,
                now,
                EntryOwners {
                    owner_workspace: Some(owner_workspace.clone()),
                    ..EntryOwners::default()
                },
            ) {
                out.push(entry);
            }
        }

        if kind_filter.is_none()
            || matches!(
                kind_filter,
                Some(GcListKindFilter::CargoTargetSubcommandCaches)
            )
        {
            for name in SUBCOMMAND_CACHE_DIRS {
                let path = row.path.join(name);
                if let Some(entry) = entry_output_for_path(
                    &path,
                    KIND_CARGO_TARGET_SUBCOMMAND_CACHES,
                    PURGE_SAFETY_DERIVED,
                    now,
                    EntryOwners {
                        owner_workspace: Some(owner_workspace.clone()),
                        ..EntryOwners::default()
                    },
                ) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

fn profile_dirs(target_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let read = match std::fs::read_dir(target_dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        out.push(entry.path());
    }
    out
}

fn is_build_script_binary(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    stem == "build-script-build" || stem.starts_with("build-script-build-")
}

// ---------------------------------------------------------------------------
// Report-only global walkers (#323 slice 5).
// ---------------------------------------------------------------------------

pub(super) fn walk_cargo_report_only(
    cargo_home: &std::path::Path,
    now: i64,
    kind_filter: Option<GcListKindFilter>,
) -> Vec<GcListEntryOutput> {
    let mut out = Vec::new();

    if kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoRegistryCache)) {
        let cache_root = cargo_home.join("registry").join("cache");
        if let Ok(registries) = std::fs::read_dir(cache_root) {
            for registry in registries.flatten() {
                if !registry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    continue;
                }
                let crates = match std::fs::read_dir(registry.path()) {
                    Ok(it) => it,
                    Err(_) => continue,
                };
                for crate_file in crates.flatten() {
                    let path = crate_file.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("crate") {
                        continue;
                    }
                    let owner = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(parse_crate_owner);
                    if let Some(entry) = entry_output_for_path(
                        &path,
                        KIND_CARGO_REGISTRY_CACHE,
                        PURGE_SAFETY_PRIMARY,
                        now,
                        EntryOwners {
                            owner_crate: owner,
                            ..EntryOwners::default()
                        },
                    ) {
                        out.push(entry);
                    }
                }
            }
        }
    }

    if kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoGitDb)) {
        let db_root = cargo_home.join("git").join("db");
        if let Ok(repos) = std::fs::read_dir(db_root) {
            for repo in repos.flatten() {
                let path = repo.path();
                let owner_repo = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string);
                if let Some(entry) = entry_output_for_path(
                    &path,
                    KIND_CARGO_GIT_DB,
                    PURGE_SAFETY_PRIMARY,
                    now,
                    EntryOwners {
                        owner_repo,
                        ..EntryOwners::default()
                    },
                ) {
                    out.push(entry);
                }
            }
        }
    }

    if kind_filter.is_none()
        || matches!(kind_filter, Some(GcListKindFilter::CargoInstalledBinaries))
    {
        let bin_root = cargo_home.join("bin");
        if let Ok(bins) = std::fs::read_dir(bin_root) {
            for bin in bins.flatten() {
                let path = bin.path();
                let owner_binary = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string);
                if let Some(entry) = entry_output_for_path(
                    &path,
                    KIND_CARGO_INSTALLED_BINARIES,
                    PURGE_SAFETY_PRIMARY,
                    now,
                    EntryOwners {
                        owner_binary,
                        ..EntryOwners::default()
                    },
                ) {
                    out.push(entry);
                }
            }
        }
    }

    out
}

pub(super) fn walk_rustup_toolchains(
    rustup_home: &std::path::Path,
    now: i64,
    kind_filter: Option<GcListKindFilter>,
) -> Vec<GcListEntryOutput> {
    if !(kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::RustupToolchain))) {
        return Vec::new();
    }

    let toolchains_root = rustup_home.join("toolchains");
    let toolchains = match std::fs::read_dir(toolchains_root) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for toolchain in toolchains.flatten() {
        let path = toolchain.path();
        let owner_toolchain = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        if let Some(entry) = entry_output_for_path(
            &path,
            KIND_RUSTUP_TOOLCHAIN,
            PURGE_SAFETY_PRIMARY,
            now,
            EntryOwners {
                owner_toolchain,
                ..EntryOwners::default()
            },
        ) {
            out.push(entry);
        }
    }
    out
}

fn entry_output_for_path(
    path: &std::path::Path,
    kind: &'static str,
    purge_safety: &'static str,
    now: i64,
    owners: EntryOwners,
) -> Option<GcListEntryOutput> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let (size_bytes, file_count) = if metadata.is_file() {
        (metadata.len(), 1)
    } else if metadata.is_dir() {
        fast_directory_size_and_files(path)
    } else {
        return None;
    };
    let last_used_unix = mtime_unix(&metadata);
    let age_seconds = now.saturating_sub(last_used_unix);
    Some(GcListEntryOutput {
        path: absolute_path_string(path),
        last_used_unix,
        age_seconds,
        age_human: crate::cache_lib::target_registry::human_age(age_seconds),
        size_bytes,
        size_human: crate::cache_lib::target_registry::human_size(size_bytes),
        file_count,
        kind,
        purge_safety,
        owner_crate: owners.owner_crate,
        owner_workspace: owners.owner_workspace,
        owner_repo: owners.owner_repo,
        owner_binary: owners.owner_binary,
        owner_toolchain: owners.owner_toolchain,
        last_used_source: Some(LAST_USED_FROM_FS_MTIME),
    })
}

fn mtime_unix(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
