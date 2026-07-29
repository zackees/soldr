//! Rendering for `soldr daemon status`.
//!
//! Extracted from `soldr_main.rs` (soldr#2023) so that file could stop
//! growing — it is over the per-file ceiling and the ratchet correctly
//! refused the addition. The status surface is a self-contained unit: one
//! `StatusInfo` in, one JSON object or one human block out.

use crate::core::SoldrPaths;
use crate::daemon::protocol::StatusInfo;

/// Print the status block in whichever form was asked for.
pub(crate) fn render(info: &StatusInfo, paths: &SoldrPaths, json: bool) {
    // Cook-index aggregate stats (issue #576). Older daemons would emit
    // `cook_stats: None` — render as zero so the surface is stable.
    let cook = info.cook_stats_or_zero();
    // soldr#1495: surface the running daemon's claimed package version and
    // whether it matches this CLI, so a version-shadow is visible here.
    let claimed_pkg = crate::daemon::broker_discovery::read_claimed_service_version(paths);
    let pkg_matches = claimed_pkg.as_deref() == Some(env!("CARGO_PKG_VERSION"));
    // soldr#2023: the daemon's limit is the one it applied at startup, so
    // compare it against what this CLI would resolve now. A running daemon
    // keeps its startup limit for life, and that used to be invisible.
    let local = crate::core::jobs::resolve_compile_jobs(
        paths
            .load_config()
            .ok()
            .and_then(|c| c.jobs.max_parallel_compiles),
    );
    let jobs_match = info.compile_jobs as usize == local.jobs;

    if json {
        let payload = serde_json::json!({
            "running": true,
            "version": info.version,
            "pkg_version": claimed_pkg,
            "pkg_version_matches_cli": pkg_matches,
            "cli_pkg_version": env!("CARGO_PKG_VERSION"),
            "pid": info.pid,
            "generation": info.generation,
            "uptime_secs": info.uptime_secs,
            "request_count": info.request_count,
            "compile_jobs": {
                "daemon": info.compile_jobs,
                "daemon_source": info.compile_jobs_source,
                "cli_would_resolve": local.jobs,
                "cli_source": local.source.describe(),
                "matches": jobs_match,
            },
            "cook": {
                "entries": cook.entries,
                "total_bytes": cook.total_bytes,
                "hits_this_session": cook.hits_this_session,
            },
            "ipc_burst": {
                "accepted": info.ipc_burst_stats.accepted,
                "queued": info.ipc_burst_stats.queued,
                "backpressured": info.ipc_burst_stats.backpressured,
                "busy_retries": info.ipc_burst_stats.busy_retries,
                "queue_high_water": info.ipc_burst_stats.queue_high_water,
            },
        });
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        return;
    }

    println!(
        "soldr-daemon: pid={} generation={} uptime={}s requests={} protocol={}",
        info.pid, info.generation, info.uptime_secs, info.request_count, info.version
    );
    println!(
        "  pkg version: {} (this cli: {}){}",
        claimed_pkg.as_deref().unwrap_or("unknown"),
        env!("CARGO_PKG_VERSION"),
        if pkg_matches {
            ""
        } else {
            "  [MISMATCH — stale daemon]"
        },
    );
    println!(
        "  compile jobs: {} (from {}){}",
        info.compile_jobs,
        info.compile_jobs_source,
        if jobs_match {
            String::new()
        } else {
            format!(
                "  [MISMATCH — this cli resolves {} from {}; \
                 `soldr daemon stop` to apply it]",
                local.jobs,
                local.source.describe()
            )
        },
    );
    println!(
        "  cook: entries={} total_bytes={} hits_this_session={}",
        cook.entries, cook.total_bytes, cook.hits_this_session
    );
    println!(
        "  ipc burst: accepted={} queued={} backpressured={} busy_retries={} queue_high_water={}",
        info.ipc_burst_stats.accepted,
        info.ipc_burst_stats.queued,
        info.ipc_burst_stats.backpressured,
        info.ipc_burst_stats.busy_retries,
        info.ipc_burst_stats.queue_high_water,
    );
}
