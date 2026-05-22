//! `soldr save` / `soldr load` CLI surface — thin wrappers around the
//! soldr-save crate's `save`/`load` functions.
//!
//! These commands consume and produce single-file `.tar.zst` archives
//! that bundle a build cache directory plus a content-verified snapshot
//! of source-file mtimes. The intent is to give any project that builds
//! from a fresh checkout (CI, hermetic sandboxes) a portable way to
//! preserve the fingerprint stability Cargo expects without resorting
//! to mtime-rewrite tricks that risk underbuilds.

use std::path::PathBuf;

use crate::cache_lib::save::{load, save, LoadOptions, SaveOptions, DEFAULT_ZSTD_LEVEL};
use clap::Args;

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// Build-cache directory whose contents should be bundled.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: PathBuf,

    /// Workspace whose tracked source files should be snapshotted.
    /// Omit to produce a cache-only archive (no mtime snapshot — load
    /// will still restore the cache but skip the replay).
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Destination archive path. Suggest `.tar.zst` suffix.
    #[arg(long, value_name = "FILE")]
    pub out: PathBuf,

    /// zstd compression level (1..=22). Default 3 — higher levels hurt
    /// wall-clock far more than they shrink transfer.
    #[arg(long, default_value_t = DEFAULT_ZSTD_LEVEL, value_name = "N")]
    pub zstd_level: i32,

    /// Thread count for parallel hashing + zstd. Default uses all
    /// available cores.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Emit a machine-readable JSON line summarising the save. Stable
    /// schema: `{"source_files","cache_files","archive_bytes","elapsed_ms"}`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LoadArgs {
    /// Archive produced by `soldr save`.
    #[arg(long, value_name = "FILE")]
    pub archive: PathBuf,

    /// Destination cache directory. Created if it doesn't exist.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: PathBuf,

    /// Workspace where source-file mtimes should be replayed. Omit
    /// to restore the cache only.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Thread count for parallel mtime replay (file hashing). Default
    /// uses all available cores.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Emit a machine-readable JSON line summarising the load. Stable
    /// schema: see `soldr save --json`'s counterpart fields.
    #[arg(long)]
    pub json: bool,
}

pub fn run_save(args: SaveArgs) -> i32 {
    let opts = SaveOptions {
        workspace: args.workspace.as_deref(),
        cache_dir: &args.cache_dir,
        out: &args.out,
        zstd_level: args.zstd_level,
        threads: args.threads,
    };
    let report = match save(&opts) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("soldr save: {err}");
            return 1;
        }
    };
    if args.json {
        println!(
            "{{\"source_files\":{},\"cache_files\":{},\"archive_bytes\":{},\"elapsed_ms\":{}}}",
            report.source_files, report.cache_files, report.archive_bytes, report.elapsed_ms,
        );
    } else {
        println!(
            "soldr save: source_files={} cache_files={} archive_bytes={} elapsed_ms={} out={}",
            report.source_files,
            report.cache_files,
            report.archive_bytes,
            report.elapsed_ms,
            args.out.display(),
        );
    }
    0
}

pub fn run_load(args: LoadArgs) -> i32 {
    let opts = LoadOptions {
        archive: &args.archive,
        cache_dir: &args.cache_dir,
        workspace: args.workspace.as_deref(),
        threads: args.threads,
    };
    let report = match load(&opts) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("soldr load: {err}");
            return 1;
        }
    };
    if args.json {
        println!(
            "{{\"cache_files_restored\":{},\"source_files_in_manifest\":{},\"mtimes_applied\":{},\"mtimes_skipped_missing\":{},\"mtimes_skipped_size_mismatch\":{},\"mtimes_skipped_modified\":{},\"elapsed_ms\":{}}}",
            report.cache_files_restored,
            report.source_files_in_manifest,
            report.mtimes_applied,
            report.mtimes_skipped_missing,
            report.mtimes_skipped_size_mismatch,
            report.mtimes_skipped_modified,
            report.elapsed_ms,
        );
    } else {
        println!(
            "soldr load: cache_files_restored={} source_files_in_manifest={} mtimes_applied={} mtimes_skipped_missing={} mtimes_skipped_size_mismatch={} mtimes_skipped_modified={} elapsed_ms={}",
            report.cache_files_restored,
            report.source_files_in_manifest,
            report.mtimes_applied,
            report.mtimes_skipped_missing,
            report.mtimes_skipped_size_mismatch,
            report.mtimes_skipped_modified,
            report.elapsed_ms,
        );
    }
    0
}
