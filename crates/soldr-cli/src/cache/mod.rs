//! Status, cache inspection, cache prune-target, version, and cache-clearing
//! commands. Extracted from `main.rs` as part of issue #339, then split into
//! submodules to keep individual files under 1000 LOC.
//!
//! The public surface — anything reachable as `crate::cache::*` — is
//! re-exported from this module so callers outside the cache module do not
//! need to know about the submodule layout.

use crate::core::{SoldrError, SoldrPaths};
use crate::zccache::managed_zccache_cache_dir;
use crate::zccache_lifecycle::run_zccache_command_raw_in_cache_dir;
use crate::{cached_active_zccache, cached_active_zccache_runtime, JSON_SCHEMA_VERSION};
use serde::Serialize;

mod install;
mod release_worktree;
mod report;
mod session;
mod trim;

pub(super) use crate::zccache_lifecycle::{
    zccache_daemon_already_stopped, zccache_output_snippet, zccache_subcommand_unsupported,
    ZCCACHE_ANALYZE_NOTE_LIMIT,
};
pub(crate) use install::run_install_zccache;
pub(crate) use release_worktree::{
    run_cache_release_worktree_command, run_cache_sweep_trash_command, sweep_trash, SweepReport,
};
pub(crate) use report::run_cache_report_command;
pub(crate) use session::{
    run_cache_flush_command, run_cache_shutdown_command, run_session_end_command,
    run_session_start_command,
};
pub(crate) use trim::{run_cache_prune_target_command, run_cache_trim_target_command, TrimProfile};

pub(crate) fn clear_zccache_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut cleared_anything = false;

    if let Some(fetch) = cached_active_zccache(&paths)? {
        let _ = crate::zccache::run_zccache_command_in_cache_dir(
            &fetch.binary_path,
            &["clear"],
            &zccache_dir,
        )?;
        println!("cleared zccache artifact cache");
        cleared_anything = true;
    }
    if zccache_dir.exists() {
        std::fs::remove_dir_all(&zccache_dir)?;
        println!("removed soldr zccache state dir: {}", zccache_dir.display());
        cleared_anything = true;
    }
    if !cleared_anything {
        println!(
            "managed zccache {} not fetched yet",
            crate::fetch::MANAGED_ZCCACHE_VERSION
        );
    }
    Ok(())
}

pub(crate) fn purge_soldr_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let mut purged_anything = false;

    purged_anything |= remove_soldr_artifact_dir("cache", &paths.cache)?;
    purged_anything |= remove_soldr_artifact_dir("bin", &paths.bin)?;

    if !purged_anything {
        println!("soldr cache is already empty: {}", paths.root.display());
    }

    Ok(())
}

fn remove_soldr_artifact_dir(label: &str, path: &std::path::Path) -> Result<bool, SoldrError> {
    if !path.exists() {
        return Ok(false);
    }

    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        std::fs::remove_dir_all(path)?;
        println!("removed soldr {label} dir: {}", path.display());
    } else {
        std::fs::remove_file(path)?;
        println!("removed soldr {label} entry: {}", path.display());
    }
    Ok(true)
}

#[derive(Serialize)]
pub(crate) struct VersionOutput {
    schema_version: u32,
    command: &'static str,
    pub(crate) soldr_version: String,
}

#[derive(Serialize)]
pub(crate) struct StatusOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    target: String,
    root_dir: String,
    cache_dir: String,
    cache_default_enabled: bool,
    cache_enabled_for_invocation: bool,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
pub(crate) struct CacheOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
pub(super) struct ZccacheStatusSnapshot {
    cache_dir: String,
    state_dir: String,
    session_log_path: String,
    session_log_present: bool,
    journal_path: String,
    journal_present: bool,
    session_stats_path: String,
    session_stats_present: bool,
    binary_path: Option<String>,
    /// Which precedence chain branch produced `binary_path`:
    /// `"pinned"`, `"local"`, `"managed"`, or `"none"`. Surfaced so
    /// `soldr cache` / `soldr status` consumers don't have to derive
    /// it from the path (issue #420).
    binary_source: &'static str,
    binary_fetched: bool,
    status_lines: Vec<String>,
    status_empty: bool,
}

pub(crate) fn version_output() -> VersionOutput {
    VersionOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "version",
        soldr_version: crate::core::version().to_string(),
    }
}

pub(crate) fn collect_status_output(cache_enabled: bool) -> Result<StatusOutput, SoldrError> {
    let target = crate::core::TargetTriple::detect()?;
    let paths = SoldrPaths::new()?;
    Ok(StatusOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        soldr_version: crate::core::version().to_string(),
        target: target.to_string(),
        root_dir: paths.root.display().to_string(),
        cache_dir: paths.cache.display().to_string(),
        cache_default_enabled: true,
        cache_enabled_for_invocation: cache_enabled,
        managed_zccache_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

pub(crate) fn collect_cache_output() -> Result<CacheOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    Ok(CacheOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache",
        soldr_version: crate::core::version().to_string(),
        managed_zccache_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

pub(super) fn collect_zccache_status(
    paths: &SoldrPaths,
) -> Result<ZccacheStatusSnapshot, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_log_path = crate::cache_lib::session_log_path(&zccache_dir);
    let session_log_present = session_log_path.exists();
    let journal_path = crate::cache_lib::session_journal_path(&zccache_dir);
    let journal_present = journal_path.exists();
    let session_stats_path = crate::cache_lib::session_stats_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();

    match cached_active_zccache_runtime(paths)? {
        Some(runtime) => {
            let fetch = runtime.to_fetch_result()?;
            // Use the raw helper so a non-zero exit with a "daemon not
            // running" stderr surfaces as success-with-empty-status rather
            // than a hard error — issue #426 made this case reachable in
            // tests by exposing a host-side pin under custom SOLDR_CACHE_DIR
            // contexts whose daemon hasn't been spawned yet. Mirrors the
            // tolerance already in place for `zccache stop` polling.
            let raw = run_zccache_command_raw_in_cache_dir(
                &fetch.binary_path,
                &["status"],
                &zccache_dir,
            )?;
            let (status_lines, status_empty) = if raw.status.success() {
                let stdout = String::from_utf8_lossy(&raw.stdout);
                let trimmed = stdout.trim();
                (
                    trimmed.lines().map(str::to_owned).collect(),
                    trimmed.is_empty(),
                )
            } else if zccache_daemon_already_stopped(&raw) {
                (Vec::new(), false)
            } else {
                let stderr = String::from_utf8_lossy(&raw.stderr);
                return Err(SoldrError::Other(format!(
                    "zccache status exited with {:?}: {}",
                    raw.status.code(),
                    stderr.trim()
                )));
            };
            Ok(ZccacheStatusSnapshot {
                cache_dir: zccache_dir.display().to_string(),
                state_dir: zccache_dir.display().to_string(),
                session_log_path: session_log_path.display().to_string(),
                session_log_present,
                journal_path: journal_path.display().to_string(),
                journal_present,
                session_stats_path: session_stats_path.display().to_string(),
                session_stats_present,
                binary_path: Some(fetch.binary_path.display().to_string()),
                binary_source: runtime.source.summary_source().as_str(),
                binary_fetched: true,
                status_lines,
                status_empty,
            })
        }
        None => Ok(ZccacheStatusSnapshot {
            cache_dir: zccache_dir.display().to_string(),
            state_dir: zccache_dir.display().to_string(),
            session_log_path: session_log_path.display().to_string(),
            session_log_present,
            journal_path: journal_path.display().to_string(),
            journal_present,
            session_stats_path: session_stats_path.display().to_string(),
            session_stats_present,
            binary_path: None,
            binary_source: "none",
            binary_fetched: false,
            status_lines: Vec::new(),
            status_empty: false,
        }),
    }
}

pub(crate) fn print_status_output(output: &StatusOutput) {
    println!("soldr {}", output.soldr_version);
    println!("target: {}", output.target);
    println!("root dir: {}", output.root_dir);
    println!("cache dir: {}", output.cache_dir);
    println!("cache default: enabled");
    println!(
        "cache mode: {}",
        if output.cache_enabled_for_invocation {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("zccache version: {}", output.managed_zccache_version);
    print_zccache_status_snapshot(&output.zccache);
}

pub(crate) fn print_cache_output(output: &CacheOutput) {
    print_zccache_status_snapshot(&output.zccache);
}

fn print_zccache_status_snapshot(snapshot: &ZccacheStatusSnapshot) {
    println!("soldr zccache cache dir: {}", snapshot.cache_dir);
    println!("soldr zccache state dir: {}", snapshot.state_dir);
    println!(
        "last session log: {} ({})",
        snapshot.session_log_path,
        if snapshot.session_log_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session journal: {} ({})",
        snapshot.journal_path,
        if snapshot.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session stats: {} ({})",
        snapshot.session_stats_path,
        if snapshot.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );

    if let Some(binary_path) = &snapshot.binary_path {
        println!(
            "zccache binary: {binary_path} (source: {})",
            snapshot.binary_source
        );
        if snapshot.status_empty {
            println!("zccache status: no output");
        } else {
            for line in &snapshot.status_lines {
                println!("zccache: {line}");
            }
        }
    } else {
        println!(
            "zccache binary: not fetched yet (will fetch managed zccache {} on the first cache-enabled build)",
            crate::fetch::MANAGED_ZCCACHE_VERSION
        );
    }
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<(), SoldrError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| SoldrError::Other(format!("failed to serialize JSON output: {e}")))?;
    println!();
    Ok(())
}
