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
/// Retain daemon event rows for 30 days when no daemon owns this root.
const DAEMON_EVENT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// soldr#1900: age at which an entry under `<cache>/tmp` is reclaimed.
///
/// Scratch is pinned to the cache volume so it can be swept from one place
/// instead of leaking into the OS temp dir forever. A day is comfortably
/// longer than any single build, so this never races an in-flight download
/// or an active test; anything older belongs to a process that is gone.
///
/// Reclaiming a still-wanted entry is cheap by construction -- everything
/// under scratch is either genuinely temporary or content-addressed and
/// re-derivable (e.g. the wrapper's stdin source file).
const SCRATCH_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Issue #1286 (F5): spawn the auto-GC sweep as a DETACHED PROCESS at
/// build end instead of an in-process thread at build start.
///
/// The previous design (`maybe_kick_auto_gc`, a detached thread kicked
/// from the front door right after `build_active::set(true)`) could
/// never actually sweep: the thread woke up inside an active build,
/// deferred with `reason=build_active`, and re-armed the marker — on a
/// machine where soldr only runs during builds, the log showed days of
/// continuous deferrals and a 36 GB cache. A post-build thread doesn't
/// work either: the wrapper process exits right after cargo does,
/// killing the sweep mid-flight. A detached `soldr gc auto-sweep`
/// child survives the wrapper's exit and starts with
/// `build_active == false` in its own process.
///
/// The 5-minute throttle marker still bounds the spawn frequency, so
/// steady-state builds pay one `stat` here and at most one process
/// spawn per throttle window.
pub(crate) fn maybe_spawn_auto_gc_sweeper(paths: &SoldrPaths) {
    if auto_gc_env_disabled() {
        return;
    }
    let cfg = match paths.load_config() {
        Ok(cfg) => cfg,
        Err(error) => {
            tracing::error!(%error, "not spawning auto-GC with invalid soldr config");
            return;
        }
    };
    let disk_pressure_enabled = cfg.auto_gc.enabled;
    let cook_enabled = cfg.cook.max_total_gb > 0 || cfg.cook.max_age_days > 0;
    if !disk_pressure_enabled && !cook_enabled {
        return;
    }
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    // Touch the marker before spawning so a crashing sweeper doesn't
    // cause us to immediately rerun on the next invocation.
    let _ = touch_auto_gc_marker(&marker);

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["gc", "auto-sweep"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW —
        // same detach semantics as daemon::lifecycle::spawn_detached_inner.
        const FLAGS: u32 = 0x0000_0200 | 0x0000_0008 | 0x0800_0000;
        cmd.creation_flags(FLAGS);
    }
    let _ = cmd.spawn();
}

/// Issue #1286 (F5): synchronous entry point behind the hidden
/// `soldr gc auto-sweep` verb. The spawner above already consumed the
/// throttle marker, so this runs the sweep unconditionally (the
/// `build_active` preamble check still applies inside).
pub(crate) fn run_gc_auto_sweep_command() -> Result<(), crate::core::SoldrError> {
    let paths = SoldrPaths::new()?;
    let log_path = crate::cache_lib::auto_gc_log_path(&paths);
    // soldr#1790: best-effort prune of the always-on per-build XML logs
    // (plus any legacy `.json` files from interim builds before the
    // JSON->XML conversion) to the newest `BUILD_LOG_KEEP` entries.
    // Rides the same 5-minute throttle + detached-sweeper design as the
    // rest of this pass — no new throttle is introduced.
    let deleted = crate::build_log::prune_build_logs(
        &crate::build_log::build_logs_dir(&paths),
        crate::build_log::BUILD_LOG_KEEP,
    );
    tracing::debug!(deleted, "pruned per-build XML logs");
    run_auto_gc_background(paths.root.clone(), log_path);
    Ok(())
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

/// Issue #980 L7: re-arm the throttle marker so the next wrapper
/// invocation can rerun the deferred sweep. Called when the background
/// thread bails out because a `cargo build` is active in this process.
fn rearm_auto_gc_marker(paths: &SoldrPaths) {
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(paths);
    // Best-effort: remove the marker so the next call sees the
    // throttle as expired. A failure to remove is silent — the
    // throttle window is 5 minutes, the worst case is one missed
    // sweep window.
    let _ = std::fs::remove_file(&marker);
}

fn build_activity_active(paths: &SoldrPaths) -> bool {
    if crate::cache_lib::build_active::is_active() {
        return true;
    }
    match crate::cache_lib::build_active::any_active(paths) {
        Ok(active) => active,
        Err(error) => {
            tracing::warn!(%error, "auto-GC lease probe failed closed");
            true
        }
    }
}

fn defer_for_active_build(paths: &SoldrPaths, log_path: &std::path::Path, stage: &str) {
    let _ = append_auto_gc_log_line(
        log_path,
        &format!("auto-gc status=deferred reason=build_active stage={stage}"),
    );
    rearm_auto_gc_marker(paths);
}

/// Reclaim stale entries under `<cache>/tmp` (soldr#1900).
///
/// Returns the number of entries removed. Best-effort throughout: a single
/// unremovable entry (still held open on Windows, or owned by another user)
/// must not abort the sweep or fail the GC pass.
fn sweep_stale_scratch(paths: &SoldrPaths, now_ms: i64) -> u64 {
    let root = crate::core::temp_root_for(paths);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return 0;
    };
    let mut removed = 0_u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let age_ok = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now_ms.saturating_sub(d.as_millis() as i64) > SCRATCH_TTL_MS)
            .unwrap_or(false);
        if !age_ok {
            continue;
        }
        let path = entry.path();
        let outcome = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if outcome.is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

fn run_auto_gc_background(paths_root: std::path::PathBuf, log_path: std::path::PathBuf) {
    use crate::cache_lib::auto_gc::DiskFreeProbe as _;
    let start = std::time::Instant::now();
    let paths = SoldrPaths::with_root(paths_root);

    // Issue #980 L7: cargo build is running in this same process. Yield
    // immediately so we don't compete for IO + the cargo
    // `.package-cache` mutex. Re-arm the marker so the post-build
    // wrapper invocation can pick up the deferred sweep.
    if build_activity_active(&paths) {
        tracing::debug!("auto-GC background tick deferred: build active");
        let _ = append_auto_gc_log_line(
            &log_path,
            "auto-gc status=deferred reason=build_active stage=preamble",
        );
        rearm_auto_gc_marker(&paths);
        return;
    }
    let _maintenance_lease =
        match crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                defer_for_active_build(&paths, &log_path, "root_lease");
                return;
            }
            Err(error) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc status=deferred reason=root_lease_failed error={error}"),
                );
                rearm_auto_gc_marker(&paths);
                return;
            }
        };

    // Issue #1286 (F5): record that a sweep actually STARTED. Before
    // this line the log could only ever contain deferrals and tier
    // actions, so "GC ran but found nothing to do" and "GC never ran"
    // were indistinguishable — which is how days of silent starvation
    // went unnoticed.
    let _ = append_auto_gc_log_line(&log_path, "auto-gc status=run stage=start");

    // Tier-0 (Phase 2): prune `daemon_events` rows older than 30 days
    // before any disk-pressure tiers run. Bounded, cheap, runs even when
    // the volume isn't below trigger so the event log can't grow
    // unbounded between auto-GC firings.
    // Scratch reclamation must NOT be gated on the state DB existing -- a
    // machine whose DB was never created (or was deleted) is exactly where
    // scratch accumulates unnoticed.
    let scratch_removed = sweep_stale_scratch(
        &paths,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    );
    if scratch_removed > 0 {
        let _ = append_auto_gc_log_line(
            &log_path,
            &format!("auto-gc tier=0 scratch_entries_reclaimed={scratch_removed}"),
        );
    }

    // The daemon performs the same event-retention pass in its maintenance
    // loop. When it is stopped, run the pass only under the same root lock
    // the daemon would hold, never as an opportunistic second opener.
    let db_path = crate::cache_lib::data_db_path(&paths);
    if db_path.exists()
        && crate::daemon::lifecycle::stale_daemon_occupies_endpoint(&paths).is_none()
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        match run_offline_daemon_event_prune(&paths, now_ms - DAEMON_EVENT_TTL_MS) {
            Ok(Some(removed)) if removed > 0 => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc tier=0 daemon_events_pruned={removed}"),
                );
            }
            Ok(_) => {}
            Err(error) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!("auto-gc tier=0 daemon_events_deferred error={error}"),
                );
            }
        }
    }

    let full_config = match paths.load_config() {
        Ok(config) => config,
        Err(error) => {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!("auto-gc status=deferred reason=invalid_config error={error}"),
            );
            rearm_auto_gc_marker(&paths);
            return;
        }
    };
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
        // soldr#1814 slice 2b (criterion 2 — single owning process per file).
        // `cook_evict_pass` opens `state.redb` via `cook_index`, so running it
        // here makes the CLI a second opener alongside the daemon.
        //
        // Skipping it when the daemon is up costs no coverage: the daemon runs
        // this exact pass in `maintenance::run_local_components`, driven by
        // `PRESSURE_INTERVAL` (5 min) — the same window as this sweeper's
        // `AUTO_GC_THROTTLE_SECONDS` (5 min). Its daily `Full` tick also adds
        // the absolute-age sweep, which this path never does. So the daemon's
        // coverage is a superset, not a delay.
        //
        // Use the version-blind PID-file occupancy check, NOT `is_live`.
        // `is_live` probes the optional broker and hashes the executable
        // identity — the #1832 note on `preflight_displace_stale_daemon`
        // records that costing tens of seconds — and CI caught exactly that
        // as a `PEP 517 daemon smoke (windows-x64)` failure here.
        //
        // Version-blind is also the semantically correct question: *any* live
        // soldr daemon owns state.redb, whatever protocol it speaks.
        match crate::daemon::lifecycle::stale_daemon_occupies_endpoint(&paths) {
            Some(pid) => {
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "cook-gc skipped: daemon pid={pid} owns state.redb and runs \
                         the same pass every 5 min (soldr#1814)"
                    ),
                );
            }
            None => match run_offline_cook_gc(&paths, &cook_config) {
                Ok(Some(report)) => log_cook_gc_report(&log_path, &report),
                Ok(None) => {
                    let _ = append_auto_gc_log_line(
                        &log_path,
                        "cook-gc deferred: daemon claimed root ownership during offline handoff",
                    );
                }
                Err(error) => {
                    let _ = append_auto_gc_log_line(
                        &log_path,
                        &format!("cook-gc deferred: offline ownership error={error}"),
                    );
                }
            },
        }
    }

    // `release-worktree` trash sweep (#710 follow-up). Runs every
    // throttle window so the per-volume `~/.soldr/trash-*/` buckets
    // get reclaimed without requiring the user to call
    // `soldr cache sweep-trash` manually. Tolerates per-entry failures
    // (Windows daemon may still hold handles); retries next pass.
    if let Ok(report) = crate::cache::sweep_trash(&paths) {
        if report.removed > 0 || report.retained > 0 {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "trash-sweep removed={} retained={}",
                    report.removed, report.retained,
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
                let tier2 = run_soldr_target_purge_background(
                    &paths,
                    &workspace_targets,
                    validated.min_age_secs,
                );
                last_tier = 2;
                free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
                // Per-stage counts (#705) — distinguishes
                // "no candidates" vs "candidates filtered out
                // pre-delete" so the next time someone hits 0-byte
                // tier-2 reclaim, they can immediately see WHERE
                // candidates went. registry_rows_on_volume is the
                // pre-scan view; candidates is the post-threshold +
                // post-guard view; on_volume_matched is the after-
                // intersect-with-affected-volumes view.
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=2 volume={} reclaimed_bytes={} free_gib={:.2} \
                         registry_rows_on_volume={} candidates={} skipped={} \
                         dropped_missing={} on_volume_matched={}",
                        plan.volume_key,
                        tier2.reclaimed,
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                        workspace_targets.len(),
                        tier2.candidates,
                        tier2.skipped,
                        tier2.dropped_missing,
                        tier2.on_volume_matched,
                    ),
                );
            }
        }

        // Tier 3: aggressive cargo GC (clamped to min_age_secs).
        // #705: probe for nightly before invoking — the unstable
        // `-Zgc` flag requires it, and without it every Tier-3
        // invocation exits non-zero silently. The probe lets us
        // record `skipped=true reason=no_nightly_toolchain` so the
        // user can see WHY tier 3 isn't helping and either install
        // nightly or accept that tier 3 won't fire on this box.
        if crate::cache_lib::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
            && cargo_volume_paths > 0
        {
            if !nightly_toolchain_available() {
                last_tier = 3;
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=3 volume={} exit_code=0 skipped=true \
                         reason=no_nightly_toolchain free_gib={:.2}",
                        plan.volume_key,
                        (free_bytes as f64) / (crate::cache_lib::auto_gc::GIB as f64),
                    ),
                );
            } else {
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

/// Run cook eviction only while holding the daemon's root-ownership lock.
///
/// This is the coordinated offline counterpart to daemon maintenance: once
/// the lock is held, a daemon cannot start between the liveness probe and
/// `cook_evict_pass` opening `state.redb`.
fn run_offline_cook_gc(
    paths: &SoldrPaths,
    config: &crate::core::CookConfig,
) -> Result<Option<crate::cache_lib::cook_gc::CookEvictReport>, std::io::Error> {
    let Some(_owner) = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)? else {
        return Ok(None);
    };
    // soldr-state-db: offline-root-owner
    Ok(Some(crate::cache_lib::cook_gc::cook_evict_pass(
        paths, config,
    )))
}

fn run_offline_daemon_event_prune(
    paths: &SoldrPaths,
    cutoff_ms: i64,
) -> Result<Option<u64>, String> {
    let Some(_owner) = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(paths)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    // soldr-state-db: offline-root-owner
    crate::daemon::db::prune_events_older_than(&crate::cache_lib::data_db_path(paths), cutoff_ms)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn log_cook_gc_report(
    log_path: &std::path::Path,
    report: &crate::cache_lib::cook_gc::CookEvictReport,
) {
    if report.time_evicted > 0
        || report.size_evicted > 0
        || report.quarantine_evicted > 0
        || report.errors > 0
    {
        let _ = append_auto_gc_log_line(
            log_path,
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

fn aggressive_cargo_gc_args(ages: &crate::cache_lib::auto_gc::CargoGcAgeSeconds) -> GcCargoArgs {
    GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_src,
        )),
        max_crate_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_crate,
        )),
        max_index_age: None,
        max_git_co_age: Some(crate::cache_lib::auto_gc::cargo_gc_duration_arg(
            ages.max_git_co,
        )),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    }
}

fn run_aggressive_cargo_gc_background(
    log_path: &std::path::Path,
    ages: &crate::cache_lib::auto_gc::CargoGcAgeSeconds,
) -> AutoGcCargoOutcome {
    let args = aggressive_cargo_gc_args(ages);
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

/// Outcome of one Tier-2 purge pass. Surfaces per-stage counts so the
/// auto-GC log distinguishes "nothing to clean" from "couldn't see
/// anything to clean" — the diagnostic gap documented in #705.
#[derive(Debug, Clone, Copy, Default)]
struct Tier2Outcome {
    /// Bytes actually deleted.
    reclaimed: u64,
    /// Eligible candidates returned by `scan` (passed threshold + guards).
    candidates: usize,
    /// Skipped rows (under threshold or guard-rejected).
    skipped: usize,
    /// Registry rows whose path no longer existed and were dropped.
    dropped_missing: usize,
    /// Candidates that survived the eligibility scan AND matched a
    /// volume in `workspace_targets`. If `candidates > 0` but
    /// `on_volume_matched == 0`, the on-volume filter ate everything
    /// (a path-normalisation drift between the registry rows and the
    /// volume-grouped paths the orchestrator handed us).
    on_volume_matched: usize,
}

/// Wall-clock ceiling for the synchronous reclaim that runs in front of a
/// blocked build (soldr#2134).
///
/// 30s is chosen to be longer than any plausible single `target/` removal and
/// far shorter than the stall it replaces. The tier only ever deletes cold,
/// off-build-tree candidates larger than 256 MB, so the common case finishes
/// well inside it and the budget is invisible.
pub(super) const BLOCK_TIER_PRUNE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

fn run_soldr_target_purge_background(
    paths: &SoldrPaths,
    workspace_targets: &[std::path::PathBuf],
    min_age_secs: u64,
) -> Tier2Outcome {
    use crate::cache_lib::gc::{parse_size, GcOptions};
    let larger_than_bytes = parse_size("256M").unwrap_or(256 * 1024 * 1024);
    // Auto-GC always honors at least the configured min-age floor.
    // We never go below 1h.
    let older_than_seconds = crate::cache_lib::auto_gc::clamp_age_to_floor(min_age_secs, 3600);
    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots: match resolve_gc_dev_roots(paths) {
            Ok(roots) => roots,
            Err(error) => {
                tracing::error!(%error, "refusing target cleanup with invalid soldr config");
                return Tier2Outcome::default();
            }
        },
        dry_run: false,
    };
    let report = match super::daemon_gc_scan(paths, &options) {
        Ok(r) => r,
        Err(_) => return Tier2Outcome::default(),
    };
    let candidates = report.candidates.len();
    let skipped = report.skipped.len();
    let dropped_missing = report.dropped_missing;
    // Filter to candidates that actually live on the affected volumes.
    let mut reclaimed = 0u64;
    let mut on_volume_matched = 0usize;
    let on_volume: std::collections::HashSet<&std::path::Path> =
        workspace_targets.iter().map(|p| p.as_path()).collect();
    // soldr#2134: bounded, because this runs *synchronously* in front of a
    // build that is already blocked. Without a deadline the stall is as long
    // as the volume is dirty -- on a machine with many large stale targets
    // that is minutes, spent deleting, with no output. Deleting is
    // best-effort by construction, so stopping early simply leaves the
    // remaining candidates for the next pass (or for the block message).
    let deadline = std::time::Instant::now() + BLOCK_TIER_PRUNE_BUDGET;
    let mut budget_exhausted = false;
    let mut removed_rows = Vec::new();
    for cand in report.candidates {
        if !on_volume.contains(cand.path.as_path()) {
            continue;
        }
        // Checked before the delete, not after: one more multi-gigabyte
        // removal past the deadline is exactly what the budget exists to
        // prevent.
        if std::time::Instant::now() >= deadline {
            budget_exhausted = true;
            break;
        }
        on_volume_matched += 1;
        let bytes = cand.size_bytes;
        let outcome = crate::cache_lib::gc::delete_candidate_dir(cand);
        if outcome.removed {
            reclaimed = reclaimed.saturating_add(bytes);
            removed_rows.push(outcome.candidate.path);
        }
    }
    if !removed_rows.is_empty() {
        let _ = super::daemon_remove_registry_rows(paths, removed_rows);
    }
    if budget_exhausted {
        eprintln!(
            "soldr: reclaim budget ({}s) reached with candidates remaining;              freed {} so far. Run `soldr gc target --purge` to finish.",
            BLOCK_TIER_PRUNE_BUDGET.as_secs(),
            crate::cache_lib::target_registry::human_size(reclaimed),
        );
    }
    Tier2Outcome {
        reclaimed,
        candidates,
        skipped,
        dropped_missing,
        on_volume_matched,
    }
}

/// Probe whether a `nightly` rustup toolchain is installed. The
/// Tier-3 aggressive `cargo clean gc` requires the unstable `-Zgc`
/// flag, so without nightly the tier silently fails with `exit_code=1`
/// (which is what #705 reported as 100% of the Tier-3 invocations in
/// the user's log). We probe via `rustup toolchain list` because it's
/// the cheapest and most-reliable check that doesn't actually spawn
/// rustc — see the auto-GC `tier=3 skipped=true reason=…` branch
/// below for how the outcome is reported.
fn nightly_toolchain_available() -> bool {
    let Ok(output) = std::process::Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .any(|line| line.trim_start().starts_with("nightly"))
}

/// Enumerate every soldr-owned path for the auto-GC orchestrator.
/// Synchronous last-chance reclaim, run when the disk watchdog is about
/// to abort a build (soldr#2134).
///
/// The reclaim mechanism already existed and already worked — the
/// reporter watched it free 118 GiB — but it lives in the detached,
/// five-minute-throttled sweeper, so it ran *after* the build had
/// already failed. The developer ate a failed command for a condition
/// soldr was about to resolve on its own.
///
/// This runs the **same** Tier-2 pass with the same guards, floors and
/// ordering, so nothing becomes eligible for deletion that was not
/// already eligible; the only change is that it happens in time to
/// matter. Returns bytes reclaimed.
///
/// Targets on other volumes are excluded (they cannot help), as is the
/// target directory of the build being blocked — deleting the cache the
/// current command is about to populate is the single most expensive
/// choice available, and it is the one the reporter actually observed.
pub(crate) fn reclaim_target_dirs_for_block(build_volume: &std::path::Path) -> u64 {
    use crate::cache_lib::auto_gc::{AutoGcPathKind, VolumeProbe};

    let Ok(paths) = SoldrPaths::new() else {
        return 0;
    };
    if auto_gc_env_disabled() {
        return 0;
    }
    let Ok(config) = paths.load_config() else {
        return 0;
    };
    if !config.auto_gc.enabled {
        return 0;
    }
    let probe = SystemVolumeProbe;
    let Some(build_key) = probe.volume_key(build_volume) else {
        return 0;
    };
    let targets: Vec<std::path::PathBuf> = enumerate_auto_gc_paths(&paths)
        .into_iter()
        .filter(|entry| matches!(entry.kind, AutoGcPathKind::WorkspaceTarget))
        .map(|entry| entry.path)
        .filter(|path| !is_same_build_tree(path, build_volume))
        .filter(|path| probe.volume_key(path).as_deref() == Some(build_key.as_str()))
        .collect();
    if targets.is_empty() {
        return 0;
    }
    let (validated, _) = crate::cache_lib::auto_gc::validate_config(&config.auto_gc);
    run_soldr_target_purge_background(&paths, &targets, validated.min_age_secs).reclaimed
}

/// Whether `candidate` is the build's own `target/` (or contains it).
///
/// The watchdog probes the target dir when it exists and the project
/// CWD when it does not, so both containment directions matter.
fn is_same_build_tree(candidate: &std::path::Path, build_volume: &std::path::Path) -> bool {
    candidate == build_volume
        || build_volume.starts_with(candidate)
        || candidate.starts_with(build_volume)
}

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
    if let Ok(rows) = super::daemon_registry_rows(paths) {
        for row in rows {
            if row.path.exists() {
                out.push(crate::cache_lib::auto_gc::AutoGcPath {
                    kind: crate::cache_lib::auto_gc::AutoGcPathKind::WorkspaceTarget,
                    path: row.path,
                });
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

#[cfg(test)]
mod scratch_sweep_tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Backdate `path` so the sweep sees it as stale without sleeping.
    fn age(path: &std::path::Path, older_than_ttl_by: Duration) {
        let when =
            SystemTime::now() - Duration::from_millis(SCRATCH_TTL_MS as u64) - older_than_ttl_by;
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
            .expect("backdate scratch entry");
    }

    crate::timed_test!(aggressive_cargo_gc_uses_cargo_accepted_duration_syntax, {
        let ages = crate::cache_lib::auto_gc::CargoGcAgeSeconds {
            max_src: 604_800,
            max_crate: 1_209_600,
            max_index: 0,
            max_git_co: 604_800,
            max_git_db: 0,
            max_download: 0,
        };
        let args = aggressive_cargo_gc_args(&ages);
        assert_eq!(args.max_src_age.as_deref(), Some("604800 seconds"));
        assert_eq!(args.max_crate_age.as_deref(), Some("1209600 seconds"));
        assert_eq!(args.max_git_co_age.as_deref(), Some("604800 seconds"));
    });

    crate::timed_test!(sweep_reclaims_stale_entries_and_keeps_fresh_ones, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let scratch = crate::core::ensure_temp_root_for(&paths);

        let stale_dir = scratch.join("stale-dir");
        std::fs::create_dir_all(&stale_dir).expect("stale dir");
        let stale_file = scratch.join("stale-file");
        std::fs::write(&stale_file, b"x").expect("stale file");
        let fresh = scratch.join("fresh-file");
        std::fs::write(&fresh, b"x").expect("fresh file");

        age(&stale_dir, Duration::from_secs(60));
        age(&stale_file, Duration::from_secs(60));

        let removed = sweep_stale_scratch(&paths, now_ms());

        assert_eq!(removed, 2, "both backdated entries must be reclaimed");
        assert!(!stale_dir.exists(), "stale directory must be removed");
        assert!(!stale_file.exists(), "stale file must be removed");
        assert!(
            fresh.exists(),
            "an entry inside the TTL must survive -- the sweep must never race \
             an in-flight download or a running test"
        );
    });

    crate::timed_test!(sweep_is_a_no_op_when_scratch_does_not_exist, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("never-created"));
        assert_eq!(sweep_stale_scratch(&paths, now_ms()), 0);
    });

    crate::timed_test!(scratch_root_tracks_the_cache_volume, {
        // The reason scratch is pinned at all: temp -> cache renames are only
        // atomic while both live on one filesystem. It sits *beside* the cache
        // rather than inside it, which is precisely why this sweep has to
        // exist -- nothing that walks `<cache>/**` will ever reclaim it.
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let scratch = crate::core::temp_root_for(&paths);
        assert!(scratch.starts_with(&paths.root), "same volume as the cache");
        assert!(!scratch.starts_with(&paths.cache), "but outside the cache");
    });

    crate::timed_test!(offline_cook_gc_requires_and_releases_root_ownership, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let config = crate::core::CookConfig {
            max_total_gb: 1,
            ..crate::core::CookConfig::default()
        };

        let owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
            .expect("acquire owner")
            .expect("root is initially unowned");
        assert!(
            run_offline_cook_gc(&paths, &config)
                .expect("offline cook probe")
                .is_none(),
            "the offline pass must not become a second state.redb owner"
        );
        drop(owner);
        assert!(
            run_offline_cook_gc(&paths, &config)
                .expect("offline cook pass")
                .is_some(),
            "the pass must resume after daemon ownership is released"
        );
    });

    crate::timed_test!(offline_event_prune_requires_and_releases_root_ownership, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
            .expect("acquire owner")
            .expect("root is initially unowned");
        assert_eq!(
            run_offline_daemon_event_prune(&paths, 0).expect("offline event probe"),
            None
        );
        drop(owner);
        assert_eq!(
            run_offline_daemon_event_prune(&paths, 0).expect("offline event prune"),
            Some(0)
        );
    });
}
