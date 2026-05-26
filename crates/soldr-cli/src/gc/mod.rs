//! Garbage-collect stale Cargo `target/` directories and orchestrate
//! the auto-GC tiers. Extracted from `main.rs` as part of issue #339;
//! split into sub-modules in 2026-05 once the production surface
//! crossed 2k LOC.
//!
//! Sub-module map:
//!
//! - [`walks`] — filesystem walkers for `cargo_registry_src` and
//!   `cargo_git_checkouts` plus the shared directory sizer.
//! - [`purge`] — confirmation prompt, parallel deletion worker pool,
//!   `gc purge --registry-src` / `--git-checkouts` commands, and the
//!   per-summary helpers used by the top-level dispatch entry.
//! - [`cargo_native`] — `gc cargo`, `gc locations`, `gc sweep` and
//!   the nightly-cargo wrapper `invoke_cargo_native_gc`.
//! - [`auto`] — auto-GC background machinery driven by the cargo
//!   front door (`maybe_kick_auto_gc`).

use crate::cache::print_json;
use crate::core::{SoldrError, SoldrPaths};
use serde::Serialize;

mod auto;
mod cargo_native;
mod purge;
mod walks;

// Re-export the public CLI surface so `crate::gc::*` keeps matching
// the call sites in `main.rs` and `cargo_front_door.rs`.
pub(crate) use auto::maybe_kick_auto_gc;
pub(crate) use cargo_native::{
    run_gc_cargo_command, run_gc_locations_command, run_gc_sweep_command,
};
pub(crate) use purge::{
    run_gc_purge_git_checkouts_command, run_gc_purge_registry_src_command,
    run_gc_purge_target_subtree_command,
};

// Items the tests file reaches through `super::*`. Keeping these
// `use`d inside `mod.rs` makes the visibility explicit and survives
// future renames without spelunking through three sub-modules.
#[cfg(test)]
use purge::{gc_purge_worker_count_for, parse_gc_purge_answer};
#[cfg(test)]
use walks::{resolve_registry_src_last_used, split_dir_name};

use purge::{
    gc_candidate_output, gc_largest_candidates, gc_total_reclaimable_bytes, print_gc_purge_result,
    print_gc_purge_scan, print_gc_summary, resolve_gc_dev_roots, run_gc_purge_candidates,
};

// ---------------------------------------------------------------------------
// soldr gc — garbage-collect stale Cargo target/ directories.
// ---------------------------------------------------------------------------

// Slice 1 of #323: scaffold the kind/purge_safety discriminator on gc JSON
// entries. Every entry emitted today is a workspace target/ dir, so every
// entry is tagged `cargo_target` / `derived`. Later slices add walkers that
// emit entries with other kinds (registry-src, git-checkouts, in-target
// subtrees, …) without further schema churn.
pub(super) const KIND_CARGO_TARGET: &str = "cargo_target";
pub(super) const KIND_CARGO_TARGET_INCREMENTAL: &str = "cargo_target_incremental";
pub(super) const KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES: &str =
    "cargo_target_build_script_binaries";
pub(super) const KIND_CARGO_TARGET_DOC: &str = "cargo_target_doc";
pub(super) const KIND_CARGO_TARGET_SUBCOMMAND_CACHES: &str = "cargo_target_subcommand_caches";
// Slice 2 of #323: `$CARGO_HOME/registry/src/<reg>/<crate>-<vers>/` extracted
// crate sources. `purge_safety: derived` — cargo regenerates from the
// matching `.crate` tarball in `registry/cache/` on demand.
pub(super) const KIND_CARGO_REGISTRY_SRC: &str = "cargo_registry_src";
pub(super) const KIND_CARGO_REGISTRY_CACHE: &str = "cargo_registry_cache";
// Slice 3 of #323: `$CARGO_HOME/git/checkouts/<repo>/<commit>/` git-source
// crate checkouts. `purge_safety: derived` — cargo regenerates by re-checking
// out the bare repo in `$CARGO_HOME/git/db/<repo>/` on demand.
pub(super) const KIND_CARGO_GIT_CHECKOUTS: &str = "cargo_git_checkouts";
pub(super) const KIND_CARGO_GIT_DB: &str = "cargo_git_db";
pub(super) const KIND_CARGO_INSTALLED_BINARIES: &str = "cargo_installed_binaries";
pub(super) const KIND_RUSTUP_TOOLCHAIN: &str = "rustup_toolchain";
pub(super) const PURGE_SAFETY_DERIVED: &str = "derived";
pub(super) const PURGE_SAFETY_PRIMARY: &str = "primary";

/// Taxonomy kinds accepted by `gc list --kind` / `gc purge --kind`
/// (#323 slice 2). The CLI's clap `ValueEnum` converts into this so the
/// gc module owns its own taxonomy without re-exporting clap types.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum GcListKindFilter {
    CargoTarget,
    CargoTargetIncremental,
    CargoTargetBuildScriptBinaries,
    CargoTargetDoc,
    CargoTargetSubcommandCaches,
    CargoRegistrySrc,
    CargoRegistryCache,
    CargoGitCheckouts,
    CargoGitDb,
    CargoInstalledBinaries,
    RustupToolchain,
}

impl From<crate::GcListKind> for GcListKindFilter {
    fn from(value: crate::GcListKind) -> Self {
        match value {
            crate::GcListKind::CargoTarget => GcListKindFilter::CargoTarget,
            crate::GcListKind::CargoTargetIncremental => GcListKindFilter::CargoTargetIncremental,
            crate::GcListKind::CargoTargetBuildScriptBinaries => {
                GcListKindFilter::CargoTargetBuildScriptBinaries
            }
            crate::GcListKind::CargoTargetDoc => GcListKindFilter::CargoTargetDoc,
            crate::GcListKind::CargoTargetSubcommandCaches => {
                GcListKindFilter::CargoTargetSubcommandCaches
            }
            crate::GcListKind::CargoRegistrySrc => GcListKindFilter::CargoRegistrySrc,
            crate::GcListKind::CargoRegistryCache => GcListKindFilter::CargoRegistryCache,
            crate::GcListKind::CargoGitCheckouts => GcListKindFilter::CargoGitCheckouts,
            crate::GcListKind::CargoGitDb => GcListKindFilter::CargoGitDb,
            crate::GcListKind::CargoInstalledBinaries => GcListKindFilter::CargoInstalledBinaries,
            crate::GcListKind::RustupToolchain => GcListKindFilter::RustupToolchain,
        }
    }
}

impl GcListKindFilter {
    pub(crate) fn kind_name(self) -> &'static str {
        match self {
            GcListKindFilter::CargoTarget => KIND_CARGO_TARGET,
            GcListKindFilter::CargoTargetIncremental => KIND_CARGO_TARGET_INCREMENTAL,
            GcListKindFilter::CargoTargetBuildScriptBinaries => {
                KIND_CARGO_TARGET_BUILD_SCRIPT_BINARIES
            }
            GcListKindFilter::CargoTargetDoc => KIND_CARGO_TARGET_DOC,
            GcListKindFilter::CargoTargetSubcommandCaches => KIND_CARGO_TARGET_SUBCOMMAND_CACHES,
            GcListKindFilter::CargoRegistrySrc => KIND_CARGO_REGISTRY_SRC,
            GcListKindFilter::CargoRegistryCache => KIND_CARGO_REGISTRY_CACHE,
            GcListKindFilter::CargoGitCheckouts => KIND_CARGO_GIT_CHECKOUTS,
            GcListKindFilter::CargoGitDb => KIND_CARGO_GIT_DB,
            GcListKindFilter::CargoInstalledBinaries => KIND_CARGO_INSTALLED_BINARIES,
            GcListKindFilter::RustupToolchain => KIND_RUSTUP_TOOLCHAIN,
        }
    }

    pub(crate) fn is_target_subtree(self) -> bool {
        matches!(
            self,
            GcListKindFilter::CargoTargetIncremental
                | GcListKindFilter::CargoTargetBuildScriptBinaries
                | GcListKindFilter::CargoTargetDoc
                | GcListKindFilter::CargoTargetSubcommandCaches
        )
    }
}

// gc list / gc summary entries follow their own schema version,
// independent of the global JSON_SCHEMA_VERSION used by other commands.
// Bumped from 1 to 2 in #323 slice 1 when kind + purge_safety were added.
pub(super) const GC_JSON_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy)]
pub(crate) enum GcMode {
    Summary,
    Purge { all: bool },
}

pub(crate) struct GcInvocation {
    pub(crate) mode: GcMode,
    pub(crate) older_than: String,
    pub(crate) larger_than: String,
    pub(crate) json: bool,
}

#[derive(Serialize)]
pub(super) struct GcCandidateOutput {
    pub(super) path: String,
    pub(super) size_bytes: u64,
    pub(super) size_human: String,
    pub(super) age_seconds: i64,
    pub(super) age_human: String,
    pub(super) eligible: bool,
    pub(super) reason: Option<String>,
    /// Taxonomy discriminator (#323 slice 1). Always `cargo_target` for
    /// now — later slices emit other kinds from new walkers.
    pub(super) kind: &'static str,
    /// Safety class for purge (#323 slice 1). Always `derived` for now.
    pub(super) purge_safety: &'static str,
}

#[derive(Serialize)]
struct GcOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    registry_path: String,
    candidate_count: usize,
    skipped_count: usize,
    total_reclaimable_bytes: u64,
    total_reclaimable_human: String,
    candidates: Vec<GcCandidateOutput>,
    largest_candidates: Vec<GcCandidateOutput>,
    skipped: Vec<GcCandidateOutput>,
    dropped_missing: usize,
    deleted_paths: Vec<String>,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    error_log_path: Option<String>,
}

pub(crate) fn run_gc_command(invocation: GcInvocation) -> Result<(), SoldrError> {
    use crate::cache_lib::gc::{
        cleanup_old_gc_logs, parse_duration, parse_size, scan, write_gc_error_log, GcOptions,
        GcPurgeSummary,
    };

    let older_than = parse_duration(&invocation.older_than).map_err(SoldrError::Other)?;
    let larger_than = parse_size(&invocation.larger_than).map_err(SoldrError::Other)?;
    let purge_all = match invocation.mode {
        GcMode::Summary => false,
        GcMode::Purge { all } => all,
    };
    let is_summary = matches!(invocation.mode, GcMode::Summary);

    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths);
    let db_path = crate::cache_lib::data_db_path(&paths);
    let registry = crate::cache_lib::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let gc_log_dir = crate::cache_lib::gc_log_dir(&paths);
    cleanup_old_gc_logs(&gc_log_dir)
        .map_err(|e| SoldrError::Other(format!("failed to clean old gc logs: {e}")))?;

    let options = GcOptions {
        older_than_seconds: older_than,
        larger_than_bytes: larger_than,
        dev_roots,
        dry_run: is_summary,
    };

    let report =
        scan(&registry, &options).map_err(|e| SoldrError::Other(format!("gc scan failed: {e}")))?;
    let total_reclaimable_bytes = gc_total_reclaimable_bytes(&report.candidates);

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut purge_summary = GcPurgeSummary::default();
    let mut error_log_path: Option<std::path::PathBuf> = None;

    if is_summary {
        if !invocation.json {
            print_gc_summary(&db_path, &report, total_reclaimable_bytes);
        }
    } else if !invocation.json {
        print_gc_purge_scan(&db_path, &report, total_reclaimable_bytes);
    }

    if !is_summary {
        purge_summary =
            run_gc_purge_candidates(&registry, &report.candidates, purge_all, invocation.json)?;
        deleted_paths = purge_summary
            .deleted_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if !purge_summary.failures.is_empty() {
            let args = std::env::args().collect::<Vec<_>>();
            let path = write_gc_error_log(&gc_log_dir, &args, &purge_summary.failures)
                .map_err(|e| SoldrError::Other(format!("failed to write gc error log: {e}")))?;
            error_log_path = Some(path);
        }
        if !invocation.json {
            print_gc_purge_result(&purge_summary, error_log_path.as_deref());
        }
    }

    if invocation.json {
        let output = GcOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: if is_summary { "summary" } else { "purge" },
            dry_run: is_summary,
            registry_path: db_path.display().to_string(),
            candidate_count: report.candidates.len(),
            skipped_count: report.skipped.len(),
            total_reclaimable_bytes,
            total_reclaimable_human: crate::cache_lib::target_registry::human_size(
                total_reclaimable_bytes,
            ),
            largest_candidates: gc_largest_candidates(&report.candidates, 5)
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            candidates: report
                .candidates
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            skipped: report
                .skipped
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            dropped_missing: report.dropped_missing,
            deleted_paths,
            selected_count: purge_summary.selected_count,
            succeeded_count: purge_summary.succeeded_count,
            failed_count: purge_summary.failed_count,
            reclaimed_bytes: purge_summary.reclaimed_bytes,
            reclaimed_human: crate::cache_lib::target_registry::human_size(
                purge_summary.reclaimed_bytes,
            ),
            error_log_path: error_log_path.map(|p| p.display().to_string()),
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
pub(super) struct GcListEntryOutput {
    pub(super) path: String,
    pub(super) last_used_unix: i64,
    pub(super) age_seconds: i64,
    pub(super) age_human: String,
    pub(super) size_bytes: u64,
    pub(super) size_human: String,
    pub(super) file_count: u64,
    /// Taxonomy discriminator (#323 slice 1). `cargo_target` is the
    /// only kind today; slice 2 also emits `cargo_registry_src`.
    pub(super) kind: &'static str,
    /// Safety class for purge (#323 slice 1). Always `derived` for now.
    pub(super) purge_safety: &'static str,
    /// `<name>@<version>` parsed from the directory name. Present on
    /// `cargo_registry_src` entries and omitted (via `skip_serializing_if`)
    /// on `cargo_target` entries that lack the concept (#323 slice 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_crate: Option<String>,
    /// Workspace owning a target-derived entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_workspace: Option<String>,
    /// Repository-ish owner for Cargo git database entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_repo: Option<String>,
    /// Binary name for Cargo-installed executable entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_binary: Option<String>,
    /// Toolchain name for rustup toolchain entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner_toolchain: Option<String>,
    /// Provenance of `last_used_unix` (#349). Present on
    /// `cargo_registry_src` entries; omitted for `cargo_target` where
    /// only mtime is available today. Values:
    ///
    /// - `"global_cache"` — cargo's own `$CARGO_HOME/.global-cache`
    ///   SQLite tracker produced the timestamp.
    /// - `"fs_mtime"` — the directory mtime, used when the tracker is
    ///   missing / locked / schema-drift / has no row for this crate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_used_source: Option<&'static str>,
}

#[derive(Serialize)]
struct GcListOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    registry_path: String,
    entry_count: usize,
    pruned_missing: usize,
    entries: Vec<GcListEntryOutput>,
}

pub(crate) fn run_gc_list_command(
    json: bool,
    kind_filter: Option<GcListKindFilter>,
) -> Result<(), SoldrError> {
    use rayon::prelude::*;
    use walks::{absolute_path_string, fast_directory_size_and_files};

    let paths = SoldrPaths::new()?;
    let db_path = crate::cache_lib::data_db_path(&paths);
    let registry = crate::cache_lib::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let rows = registry
        .list()
        .map_err(|e| SoldrError::Other(format!("gc list failed: {e}")))?;
    let now = crate::cache_lib::target_registry::current_unix_seconds()
        .map_err(|e| SoldrError::Other(format!("gc list clock error: {e}")))?;

    // Partition rows into those still on disk and those that have
    // disappeared since the registry was written. Missing rows are
    // never reported — they're swept out of the registry at the end
    // via a single batched delete.
    let (live_rows, missing_paths): (Vec<_>, Vec<_>) = rows.into_par_iter().partition_map(|row| {
        if row.path.exists() {
            rayon::iter::Either::Left(row)
        } else {
            rayon::iter::Either::Right(row.path)
        }
    });

    let include_targets =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoTarget));
    let include_registry_src =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoRegistrySrc));
    let include_git_checkouts =
        kind_filter.is_none() || matches!(kind_filter, Some(GcListKindFilter::CargoGitCheckouts));

    let mut entries: Vec<GcListEntryOutput> = if include_targets {
        live_rows
            .clone()
            .into_par_iter()
            .map(|row| {
                let (size_bytes, file_count) = fast_directory_size_and_files(&row.path);
                let age_seconds = now.saturating_sub(row.last_used);
                GcListEntryOutput {
                    path: absolute_path_string(&row.path),
                    last_used_unix: row.last_used,
                    age_seconds,
                    age_human: crate::cache_lib::target_registry::human_age(age_seconds),
                    size_bytes,
                    size_human: crate::cache_lib::target_registry::human_size(size_bytes),
                    file_count,
                    kind: KIND_CARGO_TARGET,
                    purge_safety: PURGE_SAFETY_DERIVED,
                    owner_crate: None,
                    owner_workspace: Some(
                        crate::cache_lib::target_registry::workspace_root_for_target(&row.path)
                            .display()
                            .to_string(),
                    ),
                    owner_repo: None,
                    owner_binary: None,
                    owner_toolchain: None,
                    last_used_source: None,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    if include_registry_src {
        if let Some(cargo_home) = crate::core::resolve_cargo_home() {
            entries.extend(walks::walk_cargo_registry_src(&cargo_home, now));
        }
    }

    if include_git_checkouts {
        if let Some(cargo_home) = crate::core::resolve_cargo_home() {
            entries.extend(walks::walk_cargo_git_checkouts(&cargo_home, now));
        }
    }

    entries.extend(walks::walk_cargo_target_subtrees(
        &live_rows,
        now,
        kind_filter,
    ));

    if let Some(cargo_home) = crate::core::resolve_cargo_home() {
        entries.extend(walks::walk_cargo_report_only(&cargo_home, now, kind_filter));
    }

    if let Some(rustup_home) = crate::core::resolve_rustup_home() {
        entries.extend(walks::walk_rustup_toolchains(
            &rustup_home,
            now,
            kind_filter,
        ));
    }

    let pruned_missing = registry
        .remove_many(&missing_paths)
        .map_err(|e| SoldrError::Other(format!("failed to prune missing registry rows: {e}")))?;

    if json {
        let output = GcListOutput {
            schema_version: GC_JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "list",
            registry_path: db_path.display().to_string(),
            entry_count: entries.len(),
            pruned_missing,
            entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc list: registry: {}", db_path.display());
        println!(
            "soldr gc list: {} tracked target dir{}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        );
        for entry in &entries {
            println!(
                "  {}  size={}  files={}  age={}  kind={}",
                entry.path, entry.size_human, entry.file_count, entry.age_human, entry.kind,
            );
        }
        if pruned_missing > 0 {
            println!(
                "soldr gc list: pruned {pruned_missing} missing row{} from registry",
                if pruned_missing == 1 { "" } else { "s" }
            );
        }
    }
    Ok(())
}

pub(crate) fn emit_startup_target_warning_if_due() {
    let Ok(paths) = SoldrPaths::new() else { return };
    let db_path = crate::cache_lib::data_db_path(&paths);
    if !db_path.exists() {
        return;
    }
    let Ok(registry) = crate::cache_lib::target_registry::TargetRegistry::open(&db_path) else {
        return;
    };
    let options = crate::cache_lib::gc::GcOptions {
        older_than_seconds: crate::cache_lib::target_registry::DEFAULT_STALE_AGE_SECONDS,
        larger_than_bytes: crate::cache_lib::target_registry::DEFAULT_STALE_SIZE_BYTES,
        dev_roots: resolve_gc_dev_roots(&paths),
        dry_run: true,
    };
    let marker = crate::cache_lib::gc_warning_marker_path(&paths);
    match crate::cache_lib::gc::maybe_build_startup_warning(&registry, &options, &marker) {
        Ok(Some(message)) => eprintln!("{message}"),
        Ok(None) => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests;
