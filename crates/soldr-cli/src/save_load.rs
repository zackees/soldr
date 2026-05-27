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

use crate::cache_lib::save::{
    load, read_manifest_file, read_manifest_from_archive, save, save_delta, write_manifest_file,
    LoadOptions, SaveDeltaOptions, SaveOptions, DEFAULT_ZSTD_LEVEL,
};
use clap::Args;

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// Build-cache directory whose contents should be bundled. Omit
    /// when `--mtimes-only` is set (manifest-only archive).
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Workspace whose tracked source files should be snapshotted.
    /// Omit to produce a cache-only archive (no mtime snapshot — load
    /// will still restore the cache but skip the replay). Required when
    /// `--mtimes-only` is set.
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
    /// schema: `{"source_files","cache_files","archive_bytes","elapsed_ms","mtimes_only"}`.
    #[arg(long)]
    pub json: bool,

    /// Produce a manifest-only archive — only the source-file mtime
    /// snapshot, no cache payload. Requires `--workspace`; mutually
    /// exclusive with `--cache-dir`. This is the setup-soldr
    /// `preserve-source-mtimes` feature, promoted into the soldr CLI
    /// so any wrapper can produce the same sidecar.
    #[arg(long = "mtimes-only")]
    pub mtimes_only: bool,

    /// Produce a delta archive by comparing `--cache-dir` against this
    /// protobuf manifest from a previously restored base archive.
    #[arg(long, value_name = "FILE")]
    pub delta_from_manifest: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LoadArgs {
    /// Archive produced by `soldr save`.
    #[arg(long, value_name = "FILE")]
    pub archive: PathBuf,

    /// Destination cache directory. Created if it doesn't exist. Omit
    /// when `--mtimes-only` is set.
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Workspace where source-file mtimes should be replayed. Omit
    /// to restore the cache only. Required when `--mtimes-only` is set.
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

    /// Treat the archive as a manifest-only snapshot — apply mtimes
    /// only, refuse to extract any cache entries. Requires
    /// `--workspace`; mutually exclusive with `--cache-dir`.
    #[arg(long = "mtimes-only")]
    pub mtimes_only: bool,

    /// Write the archive's protobuf manifest to this path after a
    /// successful load, for later `soldr save --delta-from-manifest`.
    #[arg(long, value_name = "FILE")]
    pub manifest_out: Option<PathBuf>,
}

pub fn run_save(args: SaveArgs) -> i32 {
    if let Some(base_manifest_path) = args.delta_from_manifest.as_deref() {
        let Some(cache_dir) = args.cache_dir.as_deref() else {
            eprintln!("soldr save: --delta-from-manifest requires --cache-dir");
            return 1;
        };
        if args.mtimes_only {
            eprintln!("soldr save: --delta-from-manifest cannot be combined with --mtimes-only");
            return 1;
        }
        let base_manifest = match read_manifest_file(base_manifest_path) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("soldr save: failed to read base manifest: {err}");
                return 1;
            }
        };
        let opts = SaveDeltaOptions {
            workspace: args.workspace.as_deref(),
            cache_dir,
            base_manifest: &base_manifest,
            out: &args.out,
            zstd_level: args.zstd_level,
            threads: args.threads,
        };
        let report = match save_delta(&opts) {
            Ok(r) => r,
            Err(err) => {
                eprintln!("soldr save: {err}");
                return 1;
            }
        };
        if args.json {
            println!(
                "{{\"source_files\":{},\"cache_files\":{},\"deleted_cache_files\":{},\"archive_bytes\":{},\"elapsed_ms\":{},\"delta\":true}}",
                report.source_files,
                report.cache_files,
                report.deleted_cache_files,
                report.archive_bytes,
                report.elapsed_ms,
            );
        } else {
            println!(
                "soldr save: source_files={} cache_files={} deleted_cache_files={} archive_bytes={} elapsed_ms={} delta=true out={}",
                report.source_files,
                report.cache_files,
                report.deleted_cache_files,
                report.archive_bytes,
                report.elapsed_ms,
                args.out.display(),
            );
        }
        return 0;
    }

    let opts = SaveOptions {
        workspace: args.workspace.as_deref(),
        cache_dir: args.cache_dir.as_deref(),
        out: &args.out,
        zstd_level: args.zstd_level,
        threads: args.threads,
        mtimes_only: args.mtimes_only,
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
            "{{\"source_files\":{},\"cache_files\":{},\"archive_bytes\":{},\"elapsed_ms\":{},\"mtimes_only\":{}}}",
            report.source_files,
            report.cache_files,
            report.archive_bytes,
            report.elapsed_ms,
            args.mtimes_only,
        );
    } else {
        println!(
            "soldr save: source_files={} cache_files={} archive_bytes={} elapsed_ms={} mtimes_only={} out={}",
            report.source_files,
            report.cache_files,
            report.archive_bytes,
            report.elapsed_ms,
            args.mtimes_only,
            args.out.display(),
        );
    }
    0
}

pub fn run_load(args: LoadArgs) -> i32 {
    let opts = LoadOptions {
        archive: &args.archive,
        cache_dir: args.cache_dir.as_deref(),
        workspace: args.workspace.as_deref(),
        threads: args.threads,
        mtimes_only: args.mtimes_only,
    };
    let report = match load(&opts) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("soldr load: {err}");
            return 1;
        }
    };
    if let Some(out) = args.manifest_out.as_deref() {
        let manifest = match read_manifest_from_archive(&args.archive) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("soldr load: failed to read manifest for --manifest-out: {err}");
                return 1;
            }
        };
        if let Err(err) = write_manifest_file(out, &manifest) {
            eprintln!("soldr load: failed to write --manifest-out: {err}");
            return 1;
        }
    }
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
