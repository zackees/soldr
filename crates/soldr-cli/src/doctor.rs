//! `soldr doctor` — drift detector for `rust-toolchain.toml`. Extracted
//! from `main.rs` as part of issue #339.

use crate::cache::print_json;
use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::defender_probe::{self, DefenderProbeState, DefenderVerdict, SCANNED_THRESHOLD_MS};
use crate::fetch::{ZccacheBinarySummary, ZccacheSource};
use crate::{apply_implicit_toolchain_homes, rustup_binary, JSON_SCHEMA_VERSION};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct DoctorComponent {
    name: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorTarget {
    triple: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorToolchain {
    channel: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorOutput {
    schema_version: u32,
    command: &'static str,
    /// Absolute path to the inspected `rust-toolchain.toml`. `None`
    /// when no manifest exists in the current working directory.
    manifest_path: Option<String>,
    /// `None` when the manifest is missing or omits `channel`.
    toolchain: Option<DoctorToolchain>,
    components: Vec<DoctorComponent>,
    targets: Vec<DoctorTarget>,
    /// Whether any declared component or target is missing from the
    /// installed rustup state. Always `false` when no manifest exists.
    drift: bool,
    missing_components: Vec<String>,
    missing_targets: Vec<String>,
    /// Which source `soldr cargo build` would actually invoke right
    /// now: `"pinned"`, `"local"`, `"managed"`, or `"none"`. Added in
    /// issue #420 so JSON consumers don't have to derive it by
    /// inspecting `pinned_zccache_active`.
    active_zccache_source: &'static str,
    /// Managed zccache state on disk. Always reports the managed-path
    /// view even when the active source is a pinned install; the
    /// pinned section below explains the override.
    managed_zccache: DoctorManagedZccache,
    /// `Some` when `soldr install-zccache` has been run.
    pinned_zccache: Option<DoctorPinnedZccache>,
    /// `true` when the active resolution is the pinned install (i.e.
    /// the managed path is superseded for the next `soldr cargo ...`).
    pinned_zccache_active: bool,
    /// Debug-info sidecar state for the running soldr binary. On
    /// Windows this points at the matching PDB directory for dump
    /// symbolication.
    soldr_debug_info: DoctorSoldrDebugInfo,
    /// Defender real-time-scan probe state for the cache directory
    /// (issue #357). `None` on non-Windows platforms.
    defender_probe: Option<DoctorDefenderProbe>,
    /// Cook-artifact cache state. `None` when neither the daemon
    /// `CookStats` nor the on-disk scan succeeds (e.g. malformed
    /// paths). Otherwise populated with whatever subset was
    /// available. (#590)
    cook: Option<DoctorCookStats>,
}

/// Cook-cache aggregate counts surfaced in `soldr doctor`. (#590)
///
/// The first three fields mirror [`crate::daemon::protocol::CookStats`]
/// and only populate when the daemon is reachable; the last two come
/// from an on-disk scan of `<soldr_paths.cache>/cook/` and always
/// populate when the directory is reachable.
#[derive(Serialize, Clone)]
struct DoctorCookStats {
    /// Rows in the daemon's `cook_index_v1` redb table. `None` when
    /// the daemon isn't running (the on-disk scan still appears).
    entries: Option<u64>,
    /// Sum of `size_bytes` across the daemon's index rows. `None`
    /// when the daemon isn't running.
    total_bytes: Option<u64>,
    /// CookLookup hits served by the current daemon since last
    /// startup. Resets across restarts. `None` when no daemon.
    hits_this_session: Option<u64>,
    /// Number of `<sha256>.tar.zst` files under the cook cache dir
    /// (excludes `.tmp/` and `.quarantine` files).
    cache_dir_artifacts: u64,
    /// Sum of file sizes counted by [`Self::cache_dir_artifacts`].
    cache_dir_bytes: u64,
    /// Absolute path to the on-disk cook cache dir.
    cache_dir: String,
}

#[derive(Serialize)]
struct DoctorDefenderProbe {
    /// `scanned`, `excluded`, or `not_applicable`.
    verdict: &'static str,
    /// Path the probe targeted (the resolved soldr cache dir).
    probed_path: String,
    /// Median write time across the probe's repeat samples, in ms.
    median_write_ms: u64,
    /// Unix timestamp the probe was run.
    probed_at_unix: u64,
    /// True when the result was just produced (and persisted) this
    /// invocation; false when the cached state was served.
    refreshed_this_run: bool,
}

#[derive(Serialize)]
struct DoctorManagedZccache {
    /// `managed`, `local`, or `none` (nothing fetched yet).
    source: &'static str,
    /// Version label. Empty when source is `none`.
    version: String,
    /// Directory whose binaries are actually executed.
    runtime_dir: String,
    /// For local builds, the path the user set in
    /// `SOLDR_ZCCACHE_LOCAL_DIR`. Null for managed builds.
    source_dir: Option<String>,
    /// Absolute path to the active CLI binary, if present.
    cli_path: Option<String>,
    /// Absolute path to the active daemon binary, if present.
    daemon_path: Option<String>,
    /// Absolute path to the active fingerprint binary, if present.
    fp_path: Option<String>,
    /// Number of debug-info sidecars present (PDBs on Windows, DWPs
    /// on Linux, dSYMs on macOS).
    debug_info_found: usize,
    /// Number of binaries we expected debug-info for (always 3).
    debug_info_expected: usize,
    /// Path to pass to `cdb -y` / `_NT_SYMBOL_PATH` when attaching.
    symbol_path: String,
}

impl DoctorManagedZccache {
    fn from_summary(summary: &ZccacheBinarySummary) -> Self {
        Self {
            source: summary.source.as_str(),
            version: summary.version.clone(),
            runtime_dir: summary.runtime_dir.display().to_string(),
            source_dir: summary.source_dir.as_ref().map(|p| p.display().to_string()),
            cli_path: summary.cli_path.as_ref().map(|p| p.display().to_string()),
            daemon_path: summary
                .daemon_path
                .as_ref()
                .map(|p| p.display().to_string()),
            fp_path: summary.fp_path.as_ref().map(|p| p.display().to_string()),
            debug_info_found: summary.debug_info_found,
            debug_info_expected: summary.debug_info_expected,
            symbol_path: summary.symbol_path.display().to_string(),
        }
    }
}

#[derive(Serialize, Clone)]
struct DoctorPinnedZccache {
    install_dir: String,
    source_kind: String,
    source_value: String,
    version: String,
    cli_path: Option<String>,
    daemon_path: Option<String>,
    fp_path: Option<String>,
    debug_info_found: usize,
    debug_info_expected: usize,
    symbol_path: String,
    drift_from_managed: bool,
    managed_version: &'static str,
}

#[derive(Serialize, Clone)]
struct DoctorSoldrDebugInfo {
    binary_path: String,
    debug_info_found: usize,
    debug_info_expected: usize,
    symbol_path: String,
}

/// Implementation of `soldr doctor`. Read-only — never invokes
/// `rustup component add` / `target add` / `toolchain install`.
pub(crate) fn run_doctor(json: bool, refresh_defender_probe: bool) -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest_path = workspace_root.join("rust-toolchain.toml");
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;
    let manifest_present = manifest_path.exists();
    let bundle = collect_zccache_bundle()?;
    let soldr_debug_info = collect_soldr_debug_info();
    let defender = collect_defender_probe(refresh_defender_probe);
    let cook = collect_cook_stats();

    let Some(channel) = manifest.channel.as_deref() else {
        if json {
            let output = DoctorOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "doctor",
                manifest_path: manifest_present.then(|| manifest_path.display().to_string()),
                toolchain: None,
                components: Vec::new(),
                targets: Vec::new(),
                drift: false,
                missing_components: Vec::new(),
                missing_targets: Vec::new(),
                active_zccache_source: bundle.active.source.as_str(),
                managed_zccache: DoctorManagedZccache::from_summary(&bundle.managed),
                pinned_zccache: bundle.pinned_doctor.clone(),
                pinned_zccache_active: bundle.pinned_active,
                soldr_debug_info: soldr_debug_info.clone(),
                defender_probe: defender_for_json(defender.as_ref()),
                cook: cook.clone(),
            };
            print_json(&output)?;
        } else if manifest_present {
            println!(
                "manifest: {} (present but no [toolchain] channel declared)",
                manifest_path.display()
            );
            print_zccache_sections(&bundle);
            print_soldr_debug_info_human(&soldr_debug_info);
            print_defender_probe_human(defender.as_ref());
            if let Some(c) = cook.as_ref() {
                print_cook_section(c);
            }
            println!("result: no manifest fields to compare; nothing to do");
        } else {
            println!(
                "no rust-toolchain.toml found in {}",
                workspace_root.display()
            );
            print_zccache_sections(&bundle);
            print_soldr_debug_info_human(&soldr_debug_info);
            print_defender_probe_human(defender.as_ref());
            if let Some(c) = cook.as_ref() {
                print_cook_section(c);
            }
            println!("result: no manifest found; nothing to compare");
        }
        return Ok(0);
    };

    let toolchain_installed = rustup_toolchain_is_installed(channel)?;

    let declared_components: Vec<String> = manifest.components.clone().unwrap_or_default();
    let declared_targets: Vec<String> = manifest.targets.clone().unwrap_or_default();

    let installed_components = if toolchain_installed && !declared_components.is_empty() {
        rustup_installed_components(channel)?
    } else {
        Vec::new()
    };
    let installed_targets = if toolchain_installed && !declared_targets.is_empty() {
        rustup_installed_targets(channel)?
    } else {
        Vec::new()
    };

    let component_rows: Vec<DoctorComponent> = declared_components
        .iter()
        .map(|declared| DoctorComponent {
            name: declared.clone(),
            installed: component_is_installed(declared, &installed_components),
        })
        .collect();
    let target_rows: Vec<DoctorTarget> = declared_targets
        .iter()
        .map(|declared| DoctorTarget {
            triple: declared.clone(),
            installed: target_is_installed(declared, &installed_targets),
        })
        .collect();

    let missing_components: Vec<String> = component_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.name.clone())
        .collect();
    let missing_targets: Vec<String> = target_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.triple.clone())
        .collect();

    let drift =
        !toolchain_installed || !missing_components.is_empty() || !missing_targets.is_empty();

    if json {
        let output = DoctorOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            manifest_path: Some(manifest_path.display().to_string()),
            toolchain: Some(DoctorToolchain {
                channel: channel.to_string(),
                installed: toolchain_installed,
            }),
            components: component_rows,
            targets: target_rows,
            drift,
            missing_components,
            missing_targets,
            active_zccache_source: bundle.active.source.as_str(),
            managed_zccache: DoctorManagedZccache::from_summary(&bundle.managed),
            pinned_zccache: bundle.pinned_doctor.clone(),
            pinned_zccache_active: bundle.pinned_active,
            soldr_debug_info: soldr_debug_info.clone(),
            defender_probe: defender_for_json(defender.as_ref()),
            cook: cook.clone(),
        };
        print_json(&output)?;
    } else {
        print_doctor_human(
            &manifest_path,
            channel,
            toolchain_installed,
            &component_rows,
            &target_rows,
            &missing_components,
            &missing_targets,
            drift,
            &bundle,
            &soldr_debug_info,
            defender.as_ref(),
            cook.as_ref(),
        );
    }

    Ok(if drift { 1 } else { 0 })
}

/// In-memory record of the defender probe result the doctor command
/// will surface. Wraps the persisted state plus a flag that tracks
/// whether this invocation produced a fresh probe (true) or reused
/// the cached one (false).
struct DefenderProbeOutcome {
    state: DefenderProbeState,
    refreshed_this_run: bool,
}

/// Collect cook-cache stats for the doctor surface (#590).
///
/// Combines two cheap probes:
/// 1. Query the running daemon via [`crate::daemon::client::status`]
///    for `CookStats` (entries, total_bytes, hits_this_session). When
///    the daemon isn't running the three fields fall through as
///    `None` and we still produce the on-disk view.
/// 2. Walk `<paths.cache>/cook/` and count `*.tar.zst` files,
///    skipping `.tmp/` and `.quarantine`. Catches drift between the
///    redb index and the artifacts on disk.
///
/// Returns `None` only when `SoldrPaths::new` fails (which would
/// already be visible elsewhere in the doctor output). Any other
/// error is swallowed — doctor is diagnostic and best-effort.
fn collect_cook_stats() -> Option<DoctorCookStats> {
    let paths = SoldrPaths::new().ok()?;
    let cook_dir = crate::cache_lib::cook_archive::cook_cache_dir(&paths);
    let sock = crate::cache_lib::daemon_sock_path(&paths);

    let from_daemon = crate::daemon::client::status(&sock).ok();
    let cook_stats = from_daemon.as_ref().map(|s| s.cook_stats_or_zero());

    // Scan the cook dir for `<sha256_hex>.tar.zst` files. Skip
    // `.tmp/` (in-flight saves) and `.quarantine` (corrupt artifacts
    // already flagged). Any I/O error → counts stay zero.
    let mut artifacts = 0u64;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&cook_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.ends_with(".quarantine") || name.starts_with('.') {
                continue;
            }
            if !name.ends_with(".tar.zst") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                artifacts += 1;
                bytes += meta.len();
            }
        }
    }

    Some(DoctorCookStats {
        entries: cook_stats.as_ref().map(|s| s.entries),
        total_bytes: cook_stats.as_ref().map(|s| s.total_bytes),
        hits_this_session: cook_stats.as_ref().map(|s| s.hits_this_session),
        cache_dir_artifacts: artifacts,
        cache_dir_bytes: bytes,
        cache_dir: cook_dir.display().to_string(),
    })
}

fn collect_soldr_debug_info() -> DoctorSoldrDebugInfo {
    let binary_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("soldr"));
    let symbol_path = binary_path
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let (debug_info_found, debug_info_expected) =
        crate::fetch::zccache::count_debug_info_sidecars(&[binary_path.as_path()]);

    DoctorSoldrDebugInfo {
        binary_path: binary_path.display().to_string(),
        debug_info_found,
        debug_info_expected,
        symbol_path: symbol_path.display().to_string(),
    }
}

/// Format a byte count for the doctor human output. Matches the
/// existing pattern of `print_managed_zccache_human` etc. — pick the
/// largest binary unit ≤ the value, two decimals.
fn fmt_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{n} B");
    }
    let kib = n / KIB;
    if kib < KIB {
        return format!("{:.2} KiB", kib);
    }
    let mib = kib / KIB;
    if mib < KIB {
        return format!("{:.2} MiB", mib);
    }
    let gib = mib / KIB;
    format!("{:.2} GiB", gib)
}

/// Render the cook section to stdout. (#590)
fn print_cook_section(cook: &DoctorCookStats) {
    println!("cook:");
    if let (Some(entries), Some(total)) = (cook.entries, cook.total_bytes) {
        println!("  entries:           {}", entries);
        println!("  total bytes:       {}", fmt_bytes(total));
        let hits = cook.hits_this_session.unwrap_or(0);
        println!("  hits this session: {}", hits);
    } else {
        println!("  (daemon not running — index counts unavailable)");
    }
    println!(
        "  cache dir:         {}  ({} artifacts, {} on disk)",
        cook.cache_dir,
        cook.cache_dir_artifacts,
        fmt_bytes(cook.cache_dir_bytes),
    );
}

/// Read the cached probe state (or run a fresh probe if forced /
/// stale / missing) and return the outcome. Errors are swallowed:
/// doctor is a diagnostic command and the probe is best-effort. On
/// non-Windows the probe always classifies as `NotApplicable`.
fn collect_defender_probe(refresh: bool) -> Option<DefenderProbeOutcome> {
    let paths = SoldrPaths::new().ok()?;
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target_dir = paths.cache.clone();
    let cached = defender_probe::read_probe_state(&paths);
    let soldr_version = env!("CARGO_PKG_VERSION");

    let reason = defender_probe::reprobe_reason(
        cached.as_ref(),
        &target_dir,
        soldr_version,
        now_unix,
        refresh,
    );

    match reason {
        None => cached.map(|state| DefenderProbeOutcome {
            state,
            refreshed_this_run: false,
        }),
        Some(_) => {
            // Skip the probe entirely outside Windows: there's no
            // Defender to measure, and writing a sample state file
            // on every Linux/macOS doctor invocation would just be
            // noise. Returning None makes the human/JSON printer
            // omit the section.
            if !cfg!(target_os = "windows") {
                return None;
            }
            let fresh = defender_probe::run_probe(&target_dir, soldr_version).ok()?;
            let _ = defender_probe::write_probe_state(&paths, &fresh);
            Some(DefenderProbeOutcome {
                state: fresh,
                refreshed_this_run: true,
            })
        }
    }
}

fn defender_for_json(outcome: Option<&DefenderProbeOutcome>) -> Option<DoctorDefenderProbe> {
    let outcome = outcome?;
    Some(DoctorDefenderProbe {
        verdict: outcome.state.verdict.as_str(),
        probed_path: outcome.state.probed_path.display().to_string(),
        median_write_ms: outcome.state.median_write_ms,
        probed_at_unix: outcome.state.probed_at_unix,
        refreshed_this_run: outcome.refreshed_this_run,
    })
}

fn print_defender_probe_human(outcome: Option<&DefenderProbeOutcome>) {
    let Some(outcome) = outcome else {
        return;
    };
    println!();
    println!("defender probe (cache directory):");
    println!("  path:          {}", outcome.state.probed_path.display());
    let age_label = if outcome.refreshed_this_run {
        "just now".to_string()
    } else {
        format_age(outcome.state.probed_at_unix)
    };
    match outcome.state.verdict {
        DefenderVerdict::NotApplicable => {
            println!("  verdict:       not applicable (non-Windows platform)");
        }
        DefenderVerdict::Excluded => {
            println!(
                "  verdict:       {} ms median write — path appears excluded from real-time scanning",
                outcome.state.median_write_ms,
            );
            println!("  probed:        {age_label}");
        }
        DefenderVerdict::Scanned => {
            println!(
                "  verdict:       {} ms median write — likely being scanned by Defender",
                outcome.state.median_write_ms,
            );
            println!("  probed:        {age_label}");
            println!(
                "  recommendation: run bench/add_defender_exclusions.ps1 as admin, or move \
                 SOLDR_CACHE_DIR onto a trusted Dev Drive (Windows 11 22H2+)"
            );
            println!("  threshold:     median > {SCANNED_THRESHOLD_MS} ms classifies as scanned");
        }
    }
}

fn format_age(probed_at_unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let delta = now.saturating_sub(probed_at_unix);
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3600 {
        let mins = delta / 60;
        return format!("{mins} min ago");
    }
    if delta < 24 * 3600 {
        let hours = delta / 3600;
        return format!("{hours} h ago");
    }
    let days = delta / (24 * 3600);
    format!("{days} d ago")
}

/// Collect zccache binary resolution info for doctor output. Read-only:
/// honors `SOLDR_ZCCACHE_LOCAL_DIR` but doesn't trigger a managed
/// fetch. The bundle includes the managed-only view (independent of
/// resolution priority) plus an optional pinned snapshot so the human
/// printer can surface both sections.
struct ZccacheDoctorBundle {
    /// Active resolution (what `soldr cargo` would use right now).
    /// Now surfaced as the leading "active zccache source:" diagnostic
    /// line so users can see at a glance which source wins resolution
    /// before reading the per-section detail (issue #420).
    active: ZccacheBinarySummary,
    /// Managed-path-only state for the "managed zccache:" section. Always
    /// reports the GitHub-Releases view, even when the pinned dir wins
    /// resolution.
    managed: ZccacheBinarySummary,
    /// Pinned-install snapshot, when `soldr install-zccache`
    /// has been run.
    pinned: Option<ZccacheBinarySummary>,
    /// JSON-friendly form of the pinned section.
    pinned_doctor: Option<DoctorPinnedZccache>,
    /// True when the active source is the pinned install.
    pinned_active: bool,
    /// Pinned sidecar (when present). Used to render source-kind / source-value.
    pinned_sidecar: Option<crate::fetch::PinnedSidecar>,
}

fn collect_zccache_bundle() -> Result<ZccacheDoctorBundle, SoldrError> {
    let paths = SoldrPaths::new()?;
    let active = crate::fetch::zccache_binary_summary(&paths)?;
    // `managed` shows whichever source the next `soldr cargo` would
    // actually use. When the pinned install is the active source the
    // human/JSON output annotates it with "(superseded by pinned)" so
    // users understand the precedence; the dedicated pinned section
    // surfaces the install dir / drift warning.
    let managed = if matches!(active.source, ZccacheSource::Pinned) {
        crate::fetch::managed_only_zccache_summary(&paths)?
    } else {
        active.clone()
    };
    let pinned = crate::fetch::pinned_zccache_summary(&paths)?;
    let pinned_sidecar = crate::fetch::read_pinned_sidecar(&paths)?;
    let pinned_active = matches!(active.source, ZccacheSource::Pinned);
    let pinned_doctor = match (pinned.as_ref(), pinned_sidecar.as_ref()) {
        (Some(summary), Some(sidecar)) => Some(DoctorPinnedZccache {
            install_dir: crate::fetch::pinned_zccache_dir(&paths)
                .display()
                .to_string(),
            source_kind: sidecar.source_kind.clone(),
            source_value: sidecar.source_value.clone(),
            version: summary.version.clone(),
            cli_path: summary.cli_path.as_ref().map(|p| p.display().to_string()),
            daemon_path: summary
                .daemon_path
                .as_ref()
                .map(|p| p.display().to_string()),
            fp_path: summary.fp_path.as_ref().map(|p| p.display().to_string()),
            debug_info_found: summary.debug_info_found,
            debug_info_expected: summary.debug_info_expected,
            symbol_path: summary.symbol_path.display().to_string(),
            drift_from_managed: crate::fetch::pinned_version_drift_from_managed(sidecar),
            managed_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
        }),
        _ => None,
    };
    Ok(ZccacheDoctorBundle {
        active,
        managed,
        pinned,
        pinned_doctor,
        pinned_active,
        pinned_sidecar,
    })
}

fn print_zccache_sections(bundle: &ZccacheDoctorBundle) {
    // Issue #420: lead with a one-liner naming the source that
    // `soldr cargo build` will actually invoke, so users debugging
    // pin-related issues don't have to cross-reference the "pinned
    // zccache:" and "managed zccache:" sections to figure out which
    // one is live.
    println!();
    println!("active zccache source: {}", bundle.active.source.as_str());
    if let (Some(summary), Some(sidecar)) = (bundle.pinned.as_ref(), bundle.pinned_sidecar.as_ref())
    {
        print_pinned_zccache_human(summary, sidecar);
    }
    print_managed_zccache_human(&bundle.managed, bundle.pinned_active);
}

fn print_soldr_debug_info_human(summary: &DoctorSoldrDebugInfo) {
    println!();
    println!("soldr debug info:");
    println!("  binary:        {}", summary.binary_path);
    let pdb_hint = if summary.debug_info_found == 0 {
        "no PDB present; build soldr with `[profile.release] debug = \"line-tables-only\"` or set CARGO_PROFILE_*_DEBUG=line-tables-only"
    } else {
        "complete"
    };
    println!(
        "  pdbs found:    {}/{} ({})",
        summary.debug_info_found, summary.debug_info_expected, pdb_hint
    );
    println!("  symbol path:   {}", summary.symbol_path);
}

fn print_pinned_zccache_human(
    summary: &ZccacheBinarySummary,
    sidecar: &crate::fetch::PinnedSidecar,
) {
    println!();
    println!("pinned zccache:");
    println!("  install dir:   {}", summary.runtime_dir.display());
    println!("  source kind:   {}", sidecar.source_kind);
    println!("  source value:  {}", sidecar.source_value);
    println!("  version:       {}", summary.version);
    match &summary.cli_path {
        Some(p) => println!("  active cli:    {}", p.display()),
        None => println!("  active cli:    <not present>"),
    }
    match &summary.daemon_path {
        Some(p) => println!("  active daemon: {}", p.display()),
        None => println!("  active daemon: <not present>"),
    }
    match &summary.fp_path {
        Some(p) => println!("  active fp:     {}", p.display()),
        None => println!("  active fp:     <not present>"),
    }
    println!("  symbol path:   {}", summary.symbol_path.display());
    if crate::fetch::pinned_version_drift_from_managed(sidecar) {
        println!(
            "  warning:       pinned version {} differs from soldr's managed default {} — \
consider `soldr install-zccache --remove` to switch back to the managed version",
            sidecar.version,
            crate::fetch::MANAGED_ZCCACHE_VERSION
        );
    }
}

fn print_managed_zccache_human(summary: &ZccacheBinarySummary, superseded_by_pinned: bool) {
    println!();
    println!("managed zccache:");
    let suffix = if superseded_by_pinned {
        " (superseded by pinned)"
    } else {
        ""
    };
    match summary.source {
        ZccacheSource::Local => {
            println!(
                "  source:        local ({}){suffix}",
                crate::fetch::ZCCACHE_LOCAL_DIR_ENV_VAR
            );
            if let Some(dir) = &summary.source_dir {
                println!("  source dir:    {}", dir.display());
            }
            if !summary.version.is_empty() {
                println!("  version:       {}", summary.version);
            }
        }
        ZccacheSource::Pinned => {
            // Shouldn't happen for the managed-only summary, but cover
            // it so the match is exhaustive and the rendering is sane
            // if the surface evolves.
            println!(
                "  source:        pinned ({}){suffix}",
                crate::fetch::PINNED_ZCCACHE_DIRNAME
            );
            if !summary.version.is_empty() {
                println!("  version:       {}", summary.version);
            }
        }
        ZccacheSource::System => {
            println!("  source:        system{suffix}");
            if !summary.version.is_empty() {
                println!("  version:       {}", summary.version);
            }
        }
        ZccacheSource::TestOverride => {
            println!("  source:        test-override{suffix}");
            if !summary.version.is_empty() {
                println!("  version:       {}", summary.version);
            }
        }
        ZccacheSource::Managed => {
            println!(
                "  source:        managed ({}){suffix}",
                crate::fetch::MANAGED_ZCCACHE_VERSION
            );
        }
        ZccacheSource::None => {
            println!(
                "  source:        managed ({}, not fetched yet){suffix}",
                crate::fetch::MANAGED_ZCCACHE_VERSION
            );
        }
    }
    println!("  runtime dir:   {}", summary.runtime_dir.display());
    match &summary.cli_path {
        Some(p) => println!("  active cli:    {}", p.display()),
        None => println!("  active cli:    <not present>"),
    }
    match &summary.daemon_path {
        Some(p) => println!("  active daemon: {}", p.display()),
        None => println!("  active daemon: <not present>"),
    }
    match &summary.fp_path {
        Some(p) => println!("  active fp:     {}", p.display()),
        None => println!("  active fp:     <not present>"),
    }
    let pdb_hint = if summary.debug_info_found == 0 {
        "no PDBs present; build zccache with `[profile.release] debug = \"line-tables-only\"` to get them"
    } else if summary.debug_info_found < summary.debug_info_expected {
        "partial — some sidecars missing"
    } else {
        "complete"
    };
    println!(
        "  pdbs found:    {}/{} ({})",
        summary.debug_info_found, summary.debug_info_expected, pdb_hint
    );
    println!("  symbol path:   {}", summary.symbol_path.display());
}

fn component_is_installed(declared: &str, installed: &[String]) -> bool {
    let prefix = format!("{declared}-");
    installed
        .iter()
        .any(|entry| entry == declared || entry.starts_with(&prefix))
}

fn target_is_installed(declared: &str, installed: &[String]) -> bool {
    installed.iter().any(|entry| entry == declared)
}

#[allow(clippy::too_many_arguments)]
fn print_doctor_human(
    manifest_path: &std::path::Path,
    channel: &str,
    toolchain_installed: bool,
    components: &[DoctorComponent],
    targets: &[DoctorTarget],
    missing_components: &[String],
    missing_targets: &[String],
    drift: bool,
    bundle: &ZccacheDoctorBundle,
    soldr_debug_info: &DoctorSoldrDebugInfo,
    defender: Option<&DefenderProbeOutcome>,
    cook: Option<&DoctorCookStats>,
) {
    println!("manifest: {}", manifest_path.display());
    println!("toolchain: {channel}");
    println!(
        "  status: {}",
        if toolchain_installed {
            "installed"
        } else {
            "MISSING"
        }
    );

    if !components.is_empty() {
        println!();
        println!("components (declared {}):", components.len());
        let width = components
            .iter()
            .map(|row| row.name.len())
            .max()
            .unwrap_or(0);
        for row in components {
            println!(
                "  {:<width$}   {}",
                row.name,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    if !targets.is_empty() {
        println!();
        println!("targets (declared {}):", targets.len());
        let width = targets
            .iter()
            .map(|row| row.triple.len())
            .max()
            .unwrap_or(0);
        for row in targets {
            println!(
                "  {:<width$}   {}",
                row.triple,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    print_zccache_sections(bundle);
    print_soldr_debug_info_human(soldr_debug_info);
    print_defender_probe_human(defender);
    if let Some(c) = cook {
        print_cook_section(c);
    }

    println!();
    if drift {
        let missing_component_count = missing_components.len();
        let missing_target_count = missing_targets.len();
        let mut parts: Vec<String> = Vec::new();
        if !toolchain_installed {
            parts.push("toolchain not installed".to_string());
        }
        if missing_component_count > 0 {
            parts.push(format!(
                "{missing_component_count} missing component{}",
                if missing_component_count == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if missing_target_count > 0 {
            parts.push(format!(
                "{missing_target_count} missing target{}",
                if missing_target_count == 1 { "" } else { "s" }
            ));
        }
        println!("result: drift detected ({})", parts.join(", "));
        println!(
            "hint: run `soldr toolchain prepare` to bring installed state in sync with manifest"
        );
    } else {
        println!("result: no drift");
    }
}

fn rustup_toolchain_is_installed(channel: &str) -> Result<bool, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["toolchain", "list"]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup toolchain list` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == channel
            || trimmed.starts_with(&format!("{channel} "))
            || trimmed.starts_with(&format!("{channel}-"))
    }))
}

fn rustup_installed_components(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup component list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn rustup_installed_targets(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup target list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn parse_rustup_list_output(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #590: cover the byte formatter at every unit boundary so the
    /// human output stays readable as cache dirs grow.
    #[test]
    fn fmt_bytes_renders_each_unit() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.00 KiB");
        assert_eq!(fmt_bytes(1536), "1.50 KiB");
        assert_eq!(fmt_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(fmt_bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }
}
