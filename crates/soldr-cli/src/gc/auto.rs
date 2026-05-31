//! Auto-GC under disk pressure (issue #323).
//!
//! Hook lives at the soldr cargo front door. On every cargo invocation
//! the wrapper consults a throttle marker and, if the throttle has
//! expired and the user hasn't opted out, spawns a detached background
//! thread that:
//!
//!   1. enumerates soldr-relevant paths and groups them by volume;
//!   2. probes free space per volume;
//!   3. runs the tiered GC plan only against volumes below the trigger;
//!   4. appends a structured line to ~/.soldr/logs/auto-gc.log.
//!
//! We deliberately spawn instead of running inline so the wrapper never
//! blocks the build. cargo's `.package-cache` mutex handles concurrent
//! invocations of `cargo clean gc` cleanly for us.

use crate::cargo_front_door::{available_space, existing_filesystem_probe_path};
use crate::core::SoldrPaths;
use crate::GcCargoArgs;

use super::cargo_native::invoke_cargo_native_gc;
use super::purge::resolve_gc_dev_roots;

const AUTO_GC_THROTTLE_SECONDS: u64 = 5 * 60;
const AUTO_GC_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const AUTO_GC_DISABLE_ENV_VAR: &str = "SOLDR_AUTO_GC_DISABLED";
/// Phase 2: retain `daemon_events` rows for 30 days. Older rows are
/// dropped by the Tier-0 step of `run_auto_gc_background`.
const DAEMON_EVENT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub(crate) fn maybe_kick_auto_gc(paths: &SoldrPaths) {
    if auto_gc_env_disabled() {
        return;
    }
    let cfg = paths.load_config();
    let disk_pressure_enabled = cfg.auto_gc.enabled;
    let cook_enabled = cfg.cook.max_total_gb > 0 || cfg.cook.max_age_days > 0;
    if !disk_pressure_enabled && !cook_enabled {
        return;
    }
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    // Touch the marker before spawning so a crashing background thread
    // doesn't cause us to immediately rerun on the next invocation.
    let _ = touch_auto_gc_marker(&marker);

    let log_path = crate::cache_lib::auto_gc_log_path(paths);
    let paths_root = paths.root.clone();
    let _ = std::thread::Builder::new()
        .name("soldr-auto-gc".to_string())
        .spawn(move || {
            run_auto_gc_background(paths_root, log_path);
        });
}

fn auto_gc_env_disabled() -> bool {
    match std::env::var(AUTO_GC_DISABLE_ENV_VAR) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

fn auto_gc_throttle_expired(marker: &std::path::Path, throttle_seconds: u64) -> bool {
    let Ok(meta) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    elapsed.as_secs() >= throttle_seconds
}

fn touch_auto_gc_marker(marker: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, "")
}

fn run_auto_gc_background(paths_root: std::path::PathBuf, log_path: std::path::PathBuf) {
    use crate::cache_lib::auto_gc::DiskFreeProbe as _;
    let start = std::time::Instant::now();
    let paths = SoldrPaths::with_root(paths_root);

    // Tier-0 (Phase 2): prune `daemon_events` rows older than 30 days
    // before any disk-pressure tiers run. Bounded, cheap, runs even when
    // the volume isn't below trigger so the event log can't grow
    // unbounded between auto-GC firings.
    let db_path = crate::cache_lib::data_db_path(&paths);
    if db_path.exists() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let cutoff_ms = now_ms - DAEMON_EVENT_TTL_MS;
        if let Ok(removed) = crate::daemon::db::prune_events_older_than(&db_path, cutoff_ms) {
            if removed > 0 {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc tier=0 daemon_events_pruned={removed}"),
                );
            }
        }
    }

    let full_config = paths.load_config();
    let config = full_config.auto_gc.clone();
    let cook_config = full_config.cook.clone();
    let (validated, warnings) = crate::cache_lib::auto_gc::validate_config(&config);
    for warning in &warnings {
        let _ = append_auto_gc_log_line(&log_path, &format!("warning: {warning}"));
    }

    // `[cook]` auto-GC (issue #589). Independent of disk-pressure
    // tiering: cook artifacts can grow unbounded even on a volume that
    // is well above `trigger_free_gb`, so the eviction pass runs every
    // throttle window when the cook knobs are non-zero.
    if cook_config.max_total_gb > 0 || cook_config.max_age_days > 0 {
        let report = crate::cache_lib::cook_gc::cook_evict_pass(&paths, &cook_config);
        if report.time_evicted > 0
            || report.size_evicted > 0
            || report.quarantine_evicted > 0
            || report.errors > 0
        {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "cook-gc protected={} time_evicted={} size_evicted={} \
                     quarantine_evicted={} bytes_freed={} errors={}",
                    report.protected,
                    report.time_evicted,
                    report.size_evicted,
                    report.quarantine_evicted,
                    report.bytes_freed,
                    report.errors,
                ),
            );
        }
    }

    if !validated.enabled {
        // Disk-pressure tiers off, but cook-gc above may still have
        // run. Done.
        return;
    }

    let auto_paths = enumerate_auto_gc_paths(&paths);
    let probe = SystemVolumeProbe;
    let plans = crate::cache_lib::auto_gc::plan_auto_gc(&validated, &auto_paths, &probe, &probe);
    if plans.is_empty() {
        return; // Either disabled or no volume is below trigger.
    }

    for plan in &plans {
        let line = format!(
            "auto-gc volume={} free_gib={:.2} trigger_gib={} target_gib={} paths={} status=detected",
            plan.volume_key,
            (plan.free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
            validated.trigger_free_gb,
            validated.target_free_gb,
            plan.paths.len()
        );
        let _ = append_auto_gc_log_line(&log_path, &line);

        // Tier 1: conservative cargo GC (no explicit --max-*-age flags
        // so cargo uses its own conservative defaults). Only attempt
        // when the volume holds the cargo home.
        let mut last_tier = 0u8;
        let cargo_volume_paths = plan
            .paths
            .iter()
            .filter(|p| matches!(p.kind, crate::cache_lib::auto_gc::AutoGcPathKind::CargoHome))
            .count();
        if cargo_volume_paths > 0 {
            let outcome = run_conservative_cargo_gc_background(&log_path);
            last_tier = 1;
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "tier=1 volume={} exit_code={} skipped={} reason={}",
                    plan.volume_key,
                    outcome.exit_code,
                    outcome.skipped,
                    outcome.reason.as_deref().unwrap_or("ran")
                ),
            );
        }

        // Re-probe and decide whether to escalate.
        let mut free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(0);
        let target_bytes = validated
            .target_free_gb
            .saturating_mul(crate::cache_lib::auto_gc::GIB);

        // Tier 2: soldr target purge (only if volume holds workspace
        // targets and we're still under target).
        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some() {
            let workspace_targets: Vec<_> = plan
                .paths
                .iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget
                    )
                })
                .map(|p| p.path.clone())
                .collect();
            if !workspace_targets.is_empty() {
                let reclaimed = run_soldr_target_purge_background(
                    &paths,
                    &workspace_targets,
                    validated.min_age_secs,
                );
                last_tier = 2;
                free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=2 volume={} reclaimed_bytes={} free_gib={:.2}",
                        plan.volume_key,
                        reclaimed,
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    ),
                );
            }
        }

        // Tier 3: aggressive cargo GC (clamped to min_age_secs).
        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
            && cargo_volume_paths > 0
        {
            let ages =
                crate::cache_lib::auto_gc::TIER3_AGES.clamped_seconds(validated.min_age_secs);
            let outcome = run_aggressive_cargo_gc_background(&log_path, &ages);
            last_tier = 3;
            free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "tier=3 volume={} exit_code={} skipped={} reason={} free_gib={:.2}",
                    plan.volume_key,
                    outcome.exit_code,
                    outcome.skipped,
                    outcome.reason.as_deref().unwrap_or("ran"),
                    (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                ),
            );
        }

        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_none()
            && free_bytes < target_bytes
        {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "auto-gc warning volume={} free_gib={:.2} target_gib={} \
                    tiers exhausted; run `soldr gc sweep --aggressive`",
                    plan.volume_key,
                    (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    validated.target_free_gb,
                ),
            );
        }
    }

    let _ = append_auto_gc_log_line(
        &log_path,
        &format!(
            "auto-gc done elapsed_ms={} volumes={}",
            start.elapsed().as_millis(),
            plans.len(),
        ),
    );
    let _ = rotate_auto_gc_log_if_needed(&log_path, AUTO_GC_LOG_MAX_BYTES);
}

struct AutoGcCargoOutcome {
    exit_code: i32,
    skipped: bool,
    reason: Option<String>,
}

fn run_conservative_cargo_gc_background(log_path: &std::path::Path) -> AutoGcCargoOutcome {
    let args = GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: None,
        max_crate_age: None,
        max_index_age: None,
        max_git_co_age: None,
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    };
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=1 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

fn run_aggressive_cargo_gc_background(
    log_path: &std::path::Path,
    ages: &crate::cache_lib::auto_gc::CargoGcAgeSeconds,
) -> AutoGcCargoOutcome {
    let args = GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: Some(format!("{}secs", ages.max_src)),
        max_crate_age: Some(format!("{}secs", ages.max_crate)),
        max_index_age: None,
        max_git_co_age: Some(format!("{}secs", ages.max_git_co)),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    };
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=3 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

fn run_soldr_target_purge_background(
    paths: &SoldrPaths,
    workspace_targets: &[std::path::PathBuf],
    min_age_secs: u64,
) -> u64 {
    use crate::cache_lib::gc::{parse_size, scan, GcOptions};
    let db_path = crate::cache_lib::data_db_path(paths);
    let Ok(registry) = crate::cache_lib::target_registry::TargetRegistry::open(&db_path) else {
        return 0;
    };
    let larger_than_bytes = parse_size("256M").unwrap_or(256 * 1024 * 1024);
    // Auto-GC always honors at least the configured min-age floor.
    // We never go below 1h.
    let older_than_seconds = crate::cache_lib::auto_gc::clamp_age_to_floor(min_age_secs, 3600);
    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots: resolve_gc_dev_roots(paths),
        dry_run: false,
    };
    let report = match scan(&registry, &options) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    // Filter to candidates that actually live on the affected volumes.
    let mut reclaimed = 0u64;
    let on_volume: std::collections::HashSet<&std::path::Path> =
        workspace_targets.iter().map(|p| p.as_path()).collect();
    for cand in report.candidates {
        if !on_volume.contains(cand.path.as_path()) {
            continue;
        }
        let bytes = cand.size_bytes;
        let outcome = crate::cache_lib::gc::delete_candidate_dir(cand);
        if outcome.removed {
            reclaimed = reclaimed.saturating_add(bytes);
            let _ = registry.remove(&outcome.candidate.path);
        }
    }
    reclaimed
}

/// Enumerate every soldr-owned path for the auto-GC orchestrator.
fn enumerate_auto_gc_paths(paths: &SoldrPaths) -> Vec<crate::cache_lib::auto_gc::AutoGcPath> {
    let mut out: Vec<crate::cache_lib::auto_gc::AutoGcPath> = Vec::new();
    if let Some(cargo_home) = crate::core::resolve_cargo_home() {
        out.push(crate::cache_lib::auto_gc::AutoGcPath {
            kind: crate::cache_lib::auto_gc::AutoGcPathKind::CargoHome,
            path: cargo_home,
        });
    }
    if let Some(rustup_home) = crate::core::resolve_rustup_home() {
        out.push(crate::cache_lib::auto_gc::AutoGcPath {
            kind: crate::cache_lib::auto_gc::AutoGcPathKind::RustupHome,
            path: rustup_home,
        });
    }
    out.push(crate::cache_lib::auto_gc::AutoGcPath {
        kind: crate::cache_lib::auto_gc::AutoGcPathKind::SoldrCache,
        path: paths.cache.clone(),
    });
    let db_path = crate::cache_lib::data_db_path(paths);
    if db_path.exists() {
        if let Ok(registry) = crate::cache_lib::target_registry::TargetRegistry::open(&db_path) {
            if let Ok(rows) = registry.list() {
                for row in rows {
                    if row.path.exists() {
                        out.push(crate::cache_lib::auto_gc::AutoGcPath {
                            kind: crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget,
                            path: row.path,
                        });
                    }
                }
            }
        }
    }
    out
}

/// System volume probe — Windows uses the drive letter (`C`, `D`),
/// Unix uses the device id from `stat()`. Falls back to the canonical
/// path's root component when neither is available.
struct SystemVolumeProbe;

impl crate::cache_lib::auto_gc::DiskFreeProbe for SystemVolumeProbe {
    fn free_bytes(&self, path: &std::path::Path) -> Option<u64> {
        let probe = existing_filesystem_probe_path(path);
        available_space(&probe).ok()
    }
}

impl crate::cache_lib::auto_gc::VolumeProbe for SystemVolumeProbe {
    fn volume_key(&self, path: &std::path::Path) -> Option<String> {
        let probe = existing_filesystem_probe_path(path);
        volume_key_for_path(&probe)
    }
}

#[cfg(windows)]
fn volume_key_for_path(path: &std::path::Path) -> Option<String> {
    // On Windows: prefer the canonical path's drive letter (e.g. "C").
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    // Strip UNC prefix \\?\ if present.
    let trimmed = s.trim_start_matches(r"\\?\");
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0].is_ascii_alphabetic()) {
        return Some((bytes[0] as char).to_ascii_uppercase().to_string());
    }
    None
}

#[cfg(unix)]
fn volume_key_for_path(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&canonical).ok()?;
    Some(meta.dev().to_string())
}

fn append_auto_gc_log_line(log_path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    use std::io::Write as _;
    writeln!(file, "{ts} {line}")?;
    Ok(())
}

fn rotate_auto_gc_log_if_needed(log_path: &std::path::Path, max_bytes: u64) -> std::io::Result<()> {
    let meta = match std::fs::metadata(log_path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.len() < max_bytes {
        return Ok(());
    }
    let archive = log_path.with_extension("log.old");
    let _ = std::fs::remove_file(&archive);
    std::fs::rename(log_path, &archive)?;
    Ok(())
}
