#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RustPlanRestoreOutcome {
    /// No rust-plan is active (target cache disabled) — nothing restored.
    NotAttempted,
    /// A plan is active but restore was skipped (warm-restore sentinel
    /// or prepopulated-target guard). The target tree was not touched.
    Skipped,
    /// `rust-plan restore` ran; `restored_file_count` files were
    /// materialized into the target dir from the verified bundle.
    Restored { restored_file_count: u64 },
}

/// Schema for `<thin-root>/manifest.v2.json`.
///
/// Written by soldr after `zccache rust-plan save` produces the bundle.
/// Downstream tooling (e.g. setup-soldr verification jobs) reads this to
/// prove what landed in the slice without unpacking it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThinSliceManifest {
    /// Manifest format version. `2` for the file-list manifest produced by
    /// the `thin-v2` profile.
    pub(crate) schema_version: u32,
    /// Active thin-slice pruning policy when this manifest was written.
    pub(crate) cache_profile: String,
    /// Absolute path of the bundle root the entries are relative to.
    pub(crate) bundle_root: String,
    /// Timestamp of manifest emission, RFC 3339 / seconds since epoch.
    pub(crate) generated_at_unix_seconds: u64,
    /// Every file in the bundle, sorted by relative path for stable diffs.
    pub(crate) files: Vec<ThinSliceManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThinSliceManifestEntry {
    /// Path relative to `bundle_root`, forward-slashed for cross-platform diffability.
    pub(crate) path: String,
    /// File size in bytes. Optional because broken symlinks etc. may not
    /// have a usable size; serialized as `null` rather than skipped so the
    /// shape is uniform across entries.
    pub(crate) size_bytes: Option<u64>,
}

pub(crate) fn write_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: Option<&'static str>,
) -> Result<(), SoldrError> {
    let profile = cache_profile.unwrap_or("thin-v1").to_string();
    if !bundle_root.exists() {
        // Nothing to manifest; skip rather than spamming an empty file.
        return Ok(());
    }
    let manifest = build_thin_manifest(bundle_root, &profile)?;
    let manifest_path = bundle_root.join(THIN_MANIFEST_FILENAME);
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| SoldrError::Other(format!("failed to serialize thin-slice manifest: {e}")))?;
    std::fs::write(&manifest_path, json)?;
    Ok(())
}

pub(crate) fn build_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: &str,
) -> Result<ThinSliceManifest, SoldrError> {
    let thread_count = resolve_bundle_walk_thread_count(
        &std::env::var(TARGET_CACHE_TAR_THREADS_ENV_VAR).unwrap_or_default(),
    )?;
    let mut files = walk_bundle_files(bundle_root, thread_count)?;
    // Drop any prior manifest so the file list does not chase its own tail
    // across repeated saves into the same bundle directory.
    files.retain(|entry| entry.path != THIN_MANIFEST_FILENAME);
    // Sort so the manifest is byte-identical regardless of walk order
    // (sequential vs parallel must produce the same output).
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let generated_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(ThinSliceManifest {
        schema_version: 2,
        cache_profile: cache_profile.to_string(),
        bundle_root: path_string(bundle_root),
        generated_at_unix_seconds,
        files,
    })
}

/// Cap on the number of metadata-stat threads soldr will spin up for the
/// bundle walk, matching the documented zccache cap. Past ~8 threads the
/// per-file `GetFileInformation` syscall stops being the bottleneck (the
/// directory iteration becomes one), so additional workers just contend.
pub(crate) const BUNDLE_WALK_THREAD_CAP: usize = 8;

/// Resolve the reader-thread count for the bundle walk from a raw
/// `SOLDR_TARGET_CACHE_TAR_THREADS` value.
///
/// The env var has already been validated by
/// [`parse_rust_artifact_cache_tar_threads`] at the cargo front door, so
/// the raw input here is expected to be well-formed. We re-validate
/// defensively because [`build_thin_manifest`] can also run on the bare
/// `RUSTC_WRAPPER` passthrough path that does not flow through the front
/// door check.
///
/// Returns:
/// - `None` for `auto` / unset — use rayon's global pool, capped at the
///   smaller of the system parallelism and [`BUNDLE_WALK_THREAD_CAP`].
/// - `Some(1)` to force the sequential fallback (no rayon overhead).
/// - `Some(n)` for an explicit thread count, clamped to
///   `[1, BUNDLE_WALK_THREAD_CAP]`.
pub(crate) fn resolve_bundle_walk_thread_count(raw: &str) -> Result<Option<usize>, SoldrError> {
    let parsed = parse_rust_artifact_cache_tar_threads(raw)?;
    let Some(token) = parsed else {
        // Unset → auto.
        return Ok(None);
    };
    if token == "auto" {
        return Ok(None);
    }
    // parse_rust_artifact_cache_tar_threads already rejected zero / negative /
    // non-integer values, so an integer-or-bust parse here is sound.
    let n: usize = token.parse().map_err(|_| {
        SoldrError::Other(format!(
            "invalid {TARGET_CACHE_TAR_THREADS_ENV_VAR} value {raw:?}; expected `auto` or a positive integer (use `1` to disable parallelism)"
        ))
    })?;
    Ok(Some(n.clamp(1, BUNDLE_WALK_THREAD_CAP)))
}

/// Walk every file under `root` and return one [`ThinSliceManifestEntry`]
/// per regular file.
///
/// Implementation is two-phase:
/// 1. Serial directory traversal (`read_dir`) collects every file path. Per
///    `read_dir` is cheap; the per-entry cost is dominated by the metadata
///    stat in phase 2 (which on Windows pays a Defender callback per file).
/// 2. Parallel `std::fs::metadata` over the collected paths via rayon.
///    Output order is non-deterministic — the caller MUST sort.
///
/// `thread_count`:
/// - `None` → use rayon's global thread pool.
/// - `Some(1)` → fully sequential (no rayon overhead at all).
/// - `Some(n)` for `n > 1` → run inside a scoped thread pool of `n`
///   workers so the env var actually controls something soldr-side.
pub(crate) fn walk_bundle_files(
    root: &std::path::Path,
    thread_count: Option<usize>,
) -> Result<Vec<ThinSliceManifestEntry>, SoldrError> {
    // Phase 1: serial DFS collects (absolute_path, relative_string) pairs.
    let mut paths: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_bundle_file_paths(root, root, &mut paths)?;

    // Phase 2: stat each file. Sequential when only one worker is wanted,
    // rayon-parallel otherwise.
    let stat = |(path, rel): &(std::path::PathBuf, String)| -> ThinSliceManifestEntry {
        let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());
        ThinSliceManifestEntry {
            path: rel.clone(),
            size_bytes,
        }
    };

    let files = match thread_count {
        Some(1) => paths.iter().map(stat).collect(),
        Some(n) => {
            use rayon::prelude::*;
            // Build a scoped pool so the explicit thread count actually
            // bounds this walk instead of leaking onto rayon's global pool.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .thread_name(|i| format!("soldr-bundle-walk-{i}"))
                .build()
                .map_err(|e| {
                    SoldrError::Other(format!("failed to build bundle-walk thread pool: {e}"))
                })?;
            pool.install(|| paths.par_iter().map(stat).collect())
        }
        None => {
            use rayon::prelude::*;
            paths.par_iter().map(stat).collect()
        }
    };

    Ok(files)
}

/// Recursively walk `dir`, pushing `(absolute_path, root-relative string)`
/// for every regular file under it. Used by [`walk_bundle_files`] as the
/// directory-iteration phase before per-file metadata stats are fanned out.
fn collect_bundle_file_paths(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, String)>,
) -> Result<(), SoldrError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SoldrError::from(e)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_bundle_file_paths(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|_| path.clone());
            let rel_string = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel_string));
        }
    }
    Ok(())
}

fn warn_if_rust_plan_restore_incomplete(summary: &zccache::artifact::RustPlanSummary) {
    // soldr#1368: read the in-process summary directly. The
    // "artifact absent from the restored plan" signal is tracked as a
    // skip reason in the library summary.
    let absent = summary
        .skipped_reasons
        .get("artifact_absent_from_restored_plan")
        .copied()
        .unwrap_or(0);
    if absent == 0 {
        return;
    }
    let restored = summary.restored_file_count.to_string();
    eprintln!(
        "soldr warning: rust-plan restore is partial \
         (artifact_absent_from_restored_plan={absent}, restored_file_count={restored}); \
         Cargo is likely to fail with missing .rmeta errors. This usually means two \
         `soldr cargo build` invocations are sharing the same --target-dir. Use a \
         distinct --target-dir for each build or clear the target directory before \
         re-running. See https://github.com/zackees/soldr/issues/228 for context."
    );
}
