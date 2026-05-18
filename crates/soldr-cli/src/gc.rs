//! Garbage-collect stale Cargo `target/` directories and orchestrate the
//! auto-GC tiers. Extracted from `main.rs` as part of issue #339.

use crate::cache::print_json;
use crate::cargo_front_door::{available_space, existing_filesystem_probe_path};
use crate::{GcCargoArgs, GcSweepArgs, JSON_SCHEMA_VERSION, SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR};
use serde::Serialize;
use soldr_core::{SoldrError, SoldrPaths};
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// soldr gc — garbage-collect stale Cargo target/ directories.
// ---------------------------------------------------------------------------

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
struct GcCandidateOutput {
    path: String,
    size_bytes: u64,
    size_human: String,
    age_seconds: i64,
    age_human: String,
    eligible: bool,
    reason: Option<String>,
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
    use soldr_cache::gc::{
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
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let gc_log_dir = soldr_cache::gc_log_dir(&paths);
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
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: if is_summary { "summary" } else { "purge" },
            dry_run: is_summary,
            registry_path: db_path.display().to_string(),
            candidate_count: report.candidates.len(),
            skipped_count: report.skipped.len(),
            total_reclaimable_bytes,
            total_reclaimable_human: soldr_cache::target_registry::human_size(
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
            reclaimed_human: soldr_cache::target_registry::human_size(
                purge_summary.reclaimed_bytes,
            ),
            error_log_path: error_log_path.map(|p| p.display().to_string()),
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct GcListEntryOutput {
    path: String,
    last_used_unix: i64,
    age_seconds: i64,
    age_human: String,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
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

fn absolute_path_string(path: &std::path::Path) -> String {
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
fn fast_directory_size_and_files(path: &std::path::Path) -> (u64, u64) {
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
                soldr_cache::target_registry::directory_size_and_files(&entry_path)
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

pub(crate) fn run_gc_list_command(json: bool) -> Result<(), SoldrError> {
    use rayon::prelude::*;

    let paths = SoldrPaths::new()?;
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let rows = registry
        .list()
        .map_err(|e| SoldrError::Other(format!("gc list failed: {e}")))?;
    let now = soldr_cache::target_registry::current_unix_seconds()
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

    let entries: Vec<GcListEntryOutput> = live_rows
        .into_par_iter()
        .map(|row| {
            let (size_bytes, file_count) = fast_directory_size_and_files(&row.path);
            let age_seconds = now.saturating_sub(row.last_used);
            GcListEntryOutput {
                path: absolute_path_string(&row.path),
                last_used_unix: row.last_used,
                age_seconds,
                age_human: soldr_cache::target_registry::human_age(age_seconds),
                size_bytes,
                size_human: soldr_cache::target_registry::human_size(size_bytes),
                file_count,
            }
        })
        .collect();

    let pruned_missing = registry
        .remove_many(&missing_paths)
        .map_err(|e| SoldrError::Other(format!("failed to prune missing registry rows: {e}")))?;

    if json {
        let output = GcListOutput {
            schema_version: JSON_SCHEMA_VERSION,
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
                "  {}  size={}  files={}  age={}",
                entry.path, entry.size_human, entry.file_count, entry.age_human,
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

fn run_gc_purge_candidates(
    registry: &soldr_cache::target_registry::TargetRegistry,
    candidates: &[soldr_cache::gc::GcCandidate],
    purge_all: bool,
    json: bool,
) -> Result<soldr_cache::gc::GcPurgeSummary, SoldrError> {
    let worker_count = gc_purge_worker_count();
    let (job_tx, job_rx) = mpsc::channel::<soldr_cache::gc::GcCandidate>();
    let (result_tx, result_rx) = mpsc::channel();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let mut workers = Vec::new();
    for idx in 0..worker_count {
        let job_rx = Arc::clone(&job_rx);
        let result_tx = result_tx.clone();
        let builder = std::thread::Builder::new().name(format!("soldr-gc-{idx}"));
        workers.push(
            builder
                .spawn(move || loop {
                    let next = {
                        let rx = job_rx.lock().expect("gc worker channel poisoned");
                        rx.recv()
                    };
                    match next {
                        Ok(candidate) => {
                            let _ =
                                result_tx.send(soldr_cache::gc::delete_candidate_dir(candidate));
                        }
                        Err(_) => break,
                    }
                })
                .map_err(|e| SoldrError::Other(format!("failed to start gc worker: {e}")))?,
        );
    }
    drop(result_tx);

    let mut selected_count = 0usize;
    let mut completed_count = 0usize;
    let mut outcomes = Vec::new();

    for cand in candidates {
        let should_delete = purge_all || prompt_gc_purge_candidate(cand);
        if !should_delete {
            continue;
        }

        selected_count += 1;
        job_tx
            .send(cand.clone())
            .map_err(|e| SoldrError::Other(format!("failed to queue gc delete: {e}")))?;
    }
    drop(job_tx);

    while completed_count < selected_count {
        match result_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(outcome) => {
                completed_count += 1;
                outcomes.push(outcome);
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| SoldrError::Other("gc worker panicked".to_string()))?;
    }

    if !json && selected_count > 0 {
        eprintln!();
    }

    soldr_cache::gc::apply_purge_outcomes(registry, outcomes)
        .map_err(|e| SoldrError::Other(format!("failed to update gc registry: {e}")))
}

fn gc_purge_worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    gc_purge_worker_count_for(available)
}

pub(crate) fn gc_purge_worker_count_for(available_parallelism: usize) -> usize {
    available_parallelism.clamp(1, 4)
}

fn print_gc_purge_progress(completed: usize, selected: usize) {
    eprint!("\rsoldr gc purge: deleting selected targets {completed}/{selected}");
    let _ = std::io::stderr().flush();
}

fn gc_total_reclaimable_bytes(candidates: &[soldr_cache::gc::GcCandidate]) -> u64 {
    candidates.iter().map(|c| c.size_bytes).sum()
}

fn print_gc_summary(
    db_path: &std::path::Path,
    report: &soldr_cache::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    println!("soldr gc: registry: {}", db_path.display());
    println!(
        "soldr gc: eligible: {} target dir{}; reclaimable: {}",
        report.candidates.len(),
        if report.candidates.len() == 1 {
            ""
        } else {
            "s"
        },
        soldr_cache::target_registry::human_size(total_reclaimable_bytes)
    );
    println!(
        "soldr gc: skipped: {}; dropped missing rows: {}",
        report.skipped.len(),
        report.dropped_missing
    );

    if report.candidates.is_empty() {
        println!("soldr gc: nothing to reclaim.");
    } else {
        println!("soldr gc: largest eligible target directories:");
        for cand in gc_largest_candidates(&report.candidates, 5) {
            println!(
                "  {}  size={}  last_used={}",
                cand.path.display(),
                soldr_cache::target_registry::human_size(cand.size_bytes),
                soldr_cache::target_registry::human_age(cand.age_seconds),
            );
        }
        println!("Run 'soldr gc purge' to delete eligible target directories.");
    }
}

fn print_gc_purge_scan(
    db_path: &std::path::Path,
    report: &soldr_cache::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    eprintln!(
        "soldr gc purge: scanned registry at {} ({} candidate dir{}, {} skipped, {} dropped missing, {} reclaimable)",
        db_path.display(),
        report.candidates.len(),
        if report.candidates.len() == 1 { "" } else { "s" },
        report.skipped.len(),
        report.dropped_missing,
        soldr_cache::target_registry::human_size(total_reclaimable_bytes)
    );

    if report.candidates.is_empty() {
        eprintln!("soldr gc purge: nothing to delete.");
    } else {
        eprintln!("soldr gc purge: candidates");
        for cand in &report.candidates {
            eprintln!(
                "  {}  size={}  age={}",
                cand.path.display(),
                soldr_cache::target_registry::human_size(cand.size_bytes),
                soldr_cache::target_registry::human_age(cand.age_seconds),
            );
        }
    }
}

fn print_gc_purge_result(
    summary: &soldr_cache::gc::GcPurgeSummary,
    error_log_path: Option<&std::path::Path>,
) {
    eprintln!(
        "soldr gc purge: selected {}; succeeded {}; failed {}; reclaimed {}",
        summary.selected_count,
        summary.succeeded_count,
        summary.failed_count,
        soldr_cache::target_registry::human_size(summary.reclaimed_bytes)
    );
    if let Some(path) = error_log_path {
        eprintln!(
            "soldr gc purge: detailed deletion errors written to {}",
            path.display()
        );
    }
}

fn gc_largest_candidates(
    candidates: &[soldr_cache::gc::GcCandidate],
    limit: usize,
) -> Vec<soldr_cache::gc::GcCandidate> {
    let mut largest = candidates.to_vec();
    largest.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    largest.truncate(limit);
    largest
}

fn gc_candidate_output(c: soldr_cache::gc::GcCandidate) -> GcCandidateOutput {
    GcCandidateOutput {
        path: c.path.display().to_string(),
        size_human: soldr_cache::target_registry::human_size(c.size_bytes),
        size_bytes: c.size_bytes,
        age_human: soldr_cache::target_registry::human_age(c.age_seconds),
        age_seconds: c.age_seconds,
        eligible: c.eligible,
        reason: c.reason,
    }
}

fn prompt_gc_purge_candidate(cand: &soldr_cache::gc::GcCandidate) -> bool {
    prompt_yes_no_default_yes(&format!(
        "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
        cand.path.display(),
        soldr_cache::target_registry::human_size(cand.size_bytes),
        soldr_cache::target_registry::human_age(cand.age_seconds),
    ))
}

fn prompt_yes_no_default_yes(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    parse_gc_purge_answer(&line)
}

pub(crate) fn parse_gc_purge_answer(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

/// Resolve the configured `gc.allowlist_roots`, falling back to
/// `~/dev` when unset.
fn resolve_gc_dev_roots(paths: &SoldrPaths) -> Vec<std::path::PathBuf> {
    let config = paths.load_config();
    let configured = config
        .gc
        .allowlist_roots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !r.trim().is_empty())
        .map(|r| soldr_core::expand_user_home(&r))
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return configured;
    }
    if let Ok(home) = soldr_core::user_home_dir() {
        return vec![home.join("dev")];
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// soldr gc cargo / gc locations / gc sweep — issue #323 manual surface.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GcCargoOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    toolchain: String,
    exit_code: i32,
    dry_run: bool,
    args: Vec<String>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    skipped: bool,
    skipped_reason: Option<String>,
}

#[derive(Serialize)]
struct GcLocationOutput {
    kind: &'static str,
    path: String,
    exists: bool,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
    owner: &'static str,
    purge_safety: &'static str,
}

#[derive(Serialize)]
struct GcLocationsOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    locations: Vec<GcLocationOutput>,
    total_size_bytes: u64,
    total_size_human: String,
}

#[derive(Serialize)]
struct GcSweepOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    cargo_gc: Option<GcCargoOutput>,
    cargo_gc_aggressive: Option<GcCargoOutput>,
    soldr_targets: Option<SoldrTargetsSummary>,
    locations: Vec<GcLocationOutput>,
    elapsed_ms: u128,
}

#[derive(Serialize, Default)]
struct SoldrTargetsSummary {
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
}

/// `soldr gc cargo` — shell out to nightly cargo's `-Zgc clean gc`.
pub(crate) fn run_gc_cargo_command(args: GcCargoArgs) -> Result<(), SoldrError> {
    let outcome = invoke_cargo_native_gc(&args, false)?;
    if args.json {
        print_json(&outcome)?;
    }
    if outcome.skipped {
        // Explicit gc cargo treats a missing nightly as a hard error,
        // unlike gc sweep which downgrades the missing toolchain to a
        // skip. We surfaced the skip JSON above for callers that care,
        // but the exit code must reflect the failure.
        return Err(SoldrError::Other(
            outcome
                .skipped_reason
                .unwrap_or_else(|| "cargo nightly GC unavailable".into()),
        ));
    }
    if outcome.exit_code != 0 {
        std::process::exit(outcome.exit_code);
    }
    Ok(())
}

/// Common implementation backing `gc cargo` and the cargo-step of
/// `gc sweep`. When `skip_when_missing` is true (sweep), a missing
/// nightly toolchain returns a `skipped = true` outcome with
/// `exit_code = 0` so the orchestrator can continue.
fn invoke_cargo_native_gc(
    args: &GcCargoArgs,
    skip_when_missing: bool,
) -> Result<GcCargoOutput, SoldrError> {
    let toolchain = resolve_gc_cargo_toolchain(args.toolchain.as_deref());

    let mut forwarded: Vec<String> = Vec::new();
    push_optional_flag(&mut forwarded, "--max-src-age", args.max_src_age.as_deref());
    push_optional_flag(
        &mut forwarded,
        "--max-crate-age",
        args.max_crate_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-index-age",
        args.max_index_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-co-age",
        args.max_git_co_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-db-age",
        args.max_git_db_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-age",
        args.max_download_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-src-size",
        args.max_src_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-crate-size",
        args.max_crate_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-size",
        args.max_git_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-size",
        args.max_download_size.as_deref(),
    );
    if args.dry_run {
        forwarded.push("--dry-run".to_string());
    }

    // Final `cargo` argv: -Zgc clean gc [forwarded...]
    let mut cargo_argv: Vec<String> =
        vec!["-Zgc".to_string(), "clean".to_string(), "gc".to_string()];
    cargo_argv.extend(forwarded.iter().cloned());

    // We invoke via `rustup run <toolchain> cargo ...` so the
    // workspace's rust-toolchain.toml override does not silently win.
    if !rustup_run_available_for(&toolchain) {
        if skip_when_missing {
            return Ok(GcCargoOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "gc",
                mode: "cargo",
                toolchain: toolchain.clone(),
                exit_code: 0,
                dry_run: args.dry_run,
                args: cargo_argv,
                stdout_bytes: 0,
                stderr_bytes: 0,
                skipped: true,
                skipped_reason: Some(format!(
                    "rustup toolchain {toolchain} not installed; skipping cargo GC"
                )),
            });
        }
        return Err(SoldrError::Other(format!(
            "rustup toolchain {toolchain} not installed; install it with `rustup toolchain install {toolchain}` or pass --toolchain <name>"
        )));
    }

    let mut command = std::process::Command::new("rustup");
    command.arg("run").arg(&toolchain).arg("cargo");
    command.args(&cargo_argv);
    soldr_core::suppress_windows_console_window(&mut command);

    eprintln!(
        "soldr gc cargo: rustup run {toolchain} cargo {}",
        cargo_argv.join(" ")
    );

    let output = command.output().map_err(|e| {
        SoldrError::Other(format!(
            "failed to invoke rustup run {toolchain} cargo: {e}"
        ))
    })?;

    // Stream cargo's output through to the user's terminal.
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);

    Ok(GcCargoOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "gc",
        mode: "cargo",
        toolchain,
        exit_code: output.status.code().unwrap_or(1),
        dry_run: args.dry_run,
        args: cargo_argv,
        stdout_bytes: output.stdout.len() as u64,
        stderr_bytes: output.stderr.len() as u64,
        skipped: false,
        skipped_reason: None,
    })
}

fn push_optional_flag(out: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(v) = value {
        out.push(format!("{flag}={v}"));
    }
}

fn resolve_gc_cargo_toolchain(flag: Option<&str>) -> String {
    flag.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "nightly".to_string())
}

/// Best-effort probe: `rustup toolchain list` must list the supplied
/// channel. We avoid actually shelling into the toolchain here because
/// `rustup run <missing> cargo --version` will install on demand,
/// which we don't want for a probe.
fn rustup_run_available_for(toolchain: &str) -> bool {
    let mut command = std::process::Command::new("rustup");
    command.args(["toolchain", "list"]);
    soldr_core::suppress_windows_console_window(&mut command);
    let output = match command.output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| line.trim().starts_with(toolchain))
}

/// `soldr gc locations` — read-only enumeration of every cache dir
/// soldr cares about, with sizes (no last-used derivation yet).
pub(crate) fn run_gc_locations_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let entries = enumerate_cache_locations(&paths);
    let total_size_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();

    if json {
        let output = GcLocationsOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "locations",
            total_size_bytes,
            total_size_human: soldr_cache::target_registry::human_size(total_size_bytes),
            locations: entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc locations:");
        println!(
            "  total tracked size: {}",
            soldr_cache::target_registry::human_size(total_size_bytes)
        );
        for entry in &entries {
            println!(
                "  [{:>8}] {}  size={}  files={}  owner={}  purge_safety={}",
                entry.kind,
                entry.path,
                entry.size_human,
                entry.file_count,
                entry.owner,
                entry.purge_safety,
            );
        }
    }
    Ok(())
}

/// Enumerate every directory soldr cares about: cargo home subdirs,
/// rustup home subdirs, soldr's own cache root, and the state.redb
/// file. Missing paths are reported with `exists = false` and zero
/// size so the JSON shape stays predictable.
fn enumerate_cache_locations(paths: &SoldrPaths) -> Vec<GcLocationOutput> {
    let mut entries: Vec<GcLocationOutput> = Vec::new();

    if let Some(cargo_home) = soldr_core::resolve_cargo_home() {
        for (kind, suffix, owner, purge_safety) in &[
            ("cargo_registry_src", "registry/src", "cargo", "regenerable"),
            (
                "cargo_registry_cache",
                "registry/cache",
                "cargo",
                "regenerable",
            ),
            (
                "cargo_registry_index",
                "registry/index",
                "cargo",
                "regenerable",
            ),
            ("cargo_git_db", "git/db", "cargo", "regenerable"),
            (
                "cargo_git_checkouts",
                "git/checkouts",
                "cargo",
                "regenerable",
            ),
        ] {
            let path = cargo_home.join(suffix);
            entries.push(gc_location_for(kind, &path, owner, purge_safety));
        }
        // .global-cache is a single file in cargo's stable tree.
        let global_cache = cargo_home.join(".global-cache");
        entries.push(gc_location_for(
            "cargo_global_cache",
            &global_cache,
            "cargo",
            "regenerable",
        ));
    }

    if let Some(rustup_home) = soldr_core::resolve_rustup_home() {
        entries.push(gc_location_for(
            "rustup_toolchains",
            &rustup_home.join("toolchains"),
            "rustup",
            "user_action",
        ));
        entries.push(gc_location_for(
            "rustup_update_hashes",
            &rustup_home.join("update-hashes"),
            "rustup",
            "regenerable",
        ));
    }

    entries.push(gc_location_for(
        "soldr_cache",
        &paths.cache,
        "soldr",
        "regenerable",
    ));
    entries.push(gc_location_for(
        "soldr_state_db",
        &paths.root.join("state.redb"),
        "soldr",
        "user_action",
    ));

    entries
}

fn gc_location_for(
    kind: &'static str,
    path: &std::path::Path,
    owner: &'static str,
    purge_safety: &'static str,
) -> GcLocationOutput {
    let exists = path.exists();
    let (size_bytes, file_count) = if exists {
        fast_directory_size_and_files(path)
    } else {
        (0, 0)
    };
    GcLocationOutput {
        kind,
        path: path.display().to_string(),
        exists,
        size_bytes,
        size_human: soldr_cache::target_registry::human_size(size_bytes),
        file_count,
        owner,
        purge_safety,
    }
}

/// `soldr gc sweep` — orchestrate locations + cargo gc + soldr target
/// purge in one go.
pub(crate) fn run_gc_sweep_command(args: GcSweepArgs) -> Result<(), SoldrError> {
    let start = std::time::Instant::now();
    let paths = SoldrPaths::new()?;
    let cargo_gc_enabled = !args.no_cargo_gc;

    // 1. Locations table (always — read-only).
    let locations = enumerate_cache_locations(&paths);

    // 2. Cargo's clean gc with conservative ages (unless disabled).
    let cargo_gc_outcome = if cargo_gc_enabled {
        // Use conservative defaults — let cargo's own ~1mo / ~3mo
        // policy decide. We don't pass any --max-*-age flags so the
        // user can configure cargo independently.
        let cargo_args = GcCargoArgs {
            dry_run: args.dry_run,
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
            json: args.json,
        };
        Some(invoke_cargo_native_gc(&cargo_args, true)?)
    } else {
        None
    };

    // 3. soldr's target purge over registered workspaces.
    let soldr_targets = if args.dry_run {
        if !args.json {
            eprintln!("soldr gc sweep: dry-run; skipping soldr target purge");
        }
        None
    } else {
        Some(run_soldr_target_purge_for_sweep(
            &paths, args.all, args.json,
        )?)
    };

    // 4. Aggressive second cargo pass.
    let cargo_gc_aggressive = if args.aggressive && cargo_gc_enabled {
        let cfg = paths.load_config();
        let floor = cfg.auto_gc.min_age_secs;
        let aggressive_args = aggressive_cargo_args(args.json, args.dry_run, floor);
        Some(invoke_cargo_native_gc(&aggressive_args, true)?)
    } else {
        None
    };

    if !args.json {
        eprintln!("soldr gc sweep: done in {} ms", start.elapsed().as_millis());
    }

    if args.json {
        let output = GcSweepOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "sweep",
            dry_run: args.dry_run,
            cargo_gc: cargo_gc_outcome,
            cargo_gc_aggressive,
            soldr_targets,
            locations,
            elapsed_ms: start.elapsed().as_millis(),
        };
        print_json(&output)?;
    }
    Ok(())
}

fn aggressive_cargo_args(json: bool, dry_run: bool, min_age_secs: u64) -> GcCargoArgs {
    // Helper: clamp `aggressive_days * 86_400` to the configured min
    // age. Express the result back in seconds (cargo accepts `s` /
    // `secs` / `seconds`).
    let clamp = |days: u64| -> String {
        let secs =
            soldr_cache::auto_gc::clamp_age_to_floor(days.saturating_mul(86_400), min_age_secs);
        format!("{secs}secs")
    };
    GcCargoArgs {
        dry_run,
        toolchain: None,
        max_src_age: Some(clamp(7)),
        max_crate_age: Some(clamp(14)),
        max_index_age: None,
        max_git_co_age: Some(clamp(7)),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json,
    }
}

fn run_soldr_target_purge_for_sweep(
    _paths: &SoldrPaths,
    purge_all: bool,
    json: bool,
) -> Result<SoldrTargetsSummary, SoldrError> {
    use soldr_cache::gc::{parse_duration, parse_size, scan, GcOptions};
    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths);
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;

    let cfg = paths.load_config();
    let older_than_seconds = soldr_cache::auto_gc::clamp_age_to_floor(
        parse_duration("10d").map_err(SoldrError::Other)?,
        cfg.auto_gc.min_age_secs,
    );
    let larger_than_bytes = parse_size("256M").map_err(SoldrError::Other)?;

    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots,
        dry_run: false,
    };
    let report =
        scan(&registry, &options).map_err(|e| SoldrError::Other(format!("gc scan failed: {e}")))?;
    if report.candidates.is_empty() {
        return Ok(SoldrTargetsSummary::default());
    }
    let purge_summary = run_gc_purge_candidates(&registry, &report.candidates, purge_all, json)?;
    Ok(SoldrTargetsSummary {
        selected_count: purge_summary.selected_count,
        succeeded_count: purge_summary.succeeded_count,
        failed_count: purge_summary.failed_count,
        reclaimed_bytes: purge_summary.reclaimed_bytes,
        reclaimed_human: soldr_cache::target_registry::human_size(purge_summary.reclaimed_bytes),
    })
}

// ---------------------------------------------------------------------------
// Auto-GC under disk pressure (issue #323).
//
// Hook lives at the soldr cargo front door. On every cargo invocation
// the wrapper consults a throttle marker and, if the throttle has
// expired and the user hasn't opted out, spawns a detached background
// thread that:
//
//   1. enumerates soldr-relevant paths and groups them by volume;
//   2. probes free space per volume;
//   3. runs the tiered GC plan only against volumes below the trigger;
//   4. appends a structured line to ~/.soldr/logs/auto-gc.log.
//
// We deliberately spawn instead of running inline so the wrapper never
// blocks the build. cargo's `.package-cache` mutex handles concurrent
// invocations of `cargo clean gc` cleanly for us.
// ---------------------------------------------------------------------------

const AUTO_GC_THROTTLE_SECONDS: u64 = 5 * 60;
const AUTO_GC_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const AUTO_GC_DISABLE_ENV_VAR: &str = "SOLDR_AUTO_GC_DISABLED";

pub(crate) fn maybe_kick_auto_gc(paths: &SoldrPaths) {
    if auto_gc_env_disabled() {
        return;
    }
    let config = paths.load_config().auto_gc;
    if !config.enabled {
        return;
    }
    let marker = soldr_cache::auto_gc_throttle_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    // Touch the marker before spawning so a crashing background thread
    // doesn't cause us to immediately rerun on the next invocation.
    let _ = touch_auto_gc_marker(&marker);

    let log_path = soldr_cache::auto_gc_log_path(paths);
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
    use soldr_cache::auto_gc::DiskFreeProbe as _;
    let start = std::time::Instant::now();
    let paths = SoldrPaths::with_root(paths_root);
    let config = paths.load_config().auto_gc;
    let (validated, warnings) = soldr_cache::auto_gc::validate_config(&config);
    for warning in &warnings {
        let _ = append_auto_gc_log_line(&log_path, &format!("warning: {warning}"));
    }

    let auto_paths = enumerate_auto_gc_paths(&paths);
    let probe = SystemVolumeProbe;
    let plans = soldr_cache::auto_gc::plan_auto_gc(&validated, &auto_paths, &probe, &probe);
    if plans.is_empty() {
        return; // Either disabled or no volume is below trigger.
    }

    for plan in &plans {
        let line = format!(
            "auto-gc volume={} free_gib={:.2} trigger_gib={} target_gib={} paths={} status=detected",
            plan.volume_key,
            (plan.free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
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
            .filter(|p| matches!(p.kind, soldr_cache::auto_gc::AutoGcPathKind::CargoHome))
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
            .saturating_mul(soldr_cache::auto_gc::GIB);

        // Tier 2: soldr target purge (only if volume holds workspace
        // targets and we're still under target).
        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some() {
            let workspace_targets: Vec<_> = plan
                .paths
                .iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        soldr_cache::auto_gc::AutoGcPathKind::WorkspaceTarget
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
                        (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
                    ),
                );
            }
        }

        // Tier 3: aggressive cargo GC (clamped to min_age_secs).
        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
            && cargo_volume_paths > 0
        {
            let ages = soldr_cache::auto_gc::TIER3_AGES.clamped_seconds(validated.min_age_secs);
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
                    (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
                ),
            );
        }

        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_none()
            && free_bytes < target_bytes
        {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "auto-gc warning volume={} free_gib={:.2} target_gib={} \
                    tiers exhausted; run `soldr gc sweep --aggressive`",
                    plan.volume_key,
                    (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
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
    ages: &soldr_cache::auto_gc::CargoGcAgeSeconds,
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
    use soldr_cache::gc::{parse_size, scan, GcOptions};
    let db_path = soldr_cache::data_db_path(paths);
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return 0;
    };
    let larger_than_bytes = parse_size("256M").unwrap_or(256 * 1024 * 1024);
    // Auto-GC always honors at least the configured min-age floor.
    // We never go below 1h.
    let older_than_seconds = soldr_cache::auto_gc::clamp_age_to_floor(min_age_secs, 3600);
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
        let outcome = soldr_cache::gc::delete_candidate_dir(cand);
        if outcome.removed {
            reclaimed = reclaimed.saturating_add(bytes);
            let _ = registry.remove(&outcome.candidate.path);
        }
    }
    reclaimed
}

/// Enumerate every soldr-owned path for the auto-GC orchestrator.
fn enumerate_auto_gc_paths(paths: &SoldrPaths) -> Vec<soldr_cache::auto_gc::AutoGcPath> {
    let mut out: Vec<soldr_cache::auto_gc::AutoGcPath> = Vec::new();
    if let Some(cargo_home) = soldr_core::resolve_cargo_home() {
        out.push(soldr_cache::auto_gc::AutoGcPath {
            kind: soldr_cache::auto_gc::AutoGcPathKind::CargoHome,
            path: cargo_home,
        });
    }
    if let Some(rustup_home) = soldr_core::resolve_rustup_home() {
        out.push(soldr_cache::auto_gc::AutoGcPath {
            kind: soldr_cache::auto_gc::AutoGcPathKind::RustupHome,
            path: rustup_home,
        });
    }
    out.push(soldr_cache::auto_gc::AutoGcPath {
        kind: soldr_cache::auto_gc::AutoGcPathKind::SoldrCache,
        path: paths.cache.clone(),
    });
    let db_path = soldr_cache::data_db_path(paths);
    if db_path.exists() {
        if let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) {
            if let Ok(rows) = registry.list() {
                for row in rows {
                    if row.path.exists() {
                        out.push(soldr_cache::auto_gc::AutoGcPath {
                            kind: soldr_cache::auto_gc::AutoGcPathKind::WorkspaceTarget,
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

impl soldr_cache::auto_gc::DiskFreeProbe for SystemVolumeProbe {
    fn free_bytes(&self, path: &std::path::Path) -> Option<u64> {
        let probe = existing_filesystem_probe_path(path);
        available_space(&probe).ok()
    }
}

impl soldr_cache::auto_gc::VolumeProbe for SystemVolumeProbe {
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

pub(crate) fn emit_startup_target_warning_if_due() {
    let Ok(paths) = SoldrPaths::new() else { return };
    let db_path = soldr_cache::data_db_path(&paths);
    if !db_path.exists() {
        return;
    }
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return;
    };
    let options = soldr_cache::gc::GcOptions {
        older_than_seconds: soldr_cache::target_registry::DEFAULT_STALE_AGE_SECONDS,
        larger_than_bytes: soldr_cache::target_registry::DEFAULT_STALE_SIZE_BYTES,
        dev_roots: resolve_gc_dev_roots(&paths),
        dry_run: true,
    };
    let marker = soldr_cache::gc_warning_marker_path(&paths);
    match soldr_cache::gc::maybe_build_startup_warning(&registry, &options, &marker) {
        Ok(Some(message)) => eprintln!("{message}"),
        Ok(None) => {}
        Err(_) => {}
    }
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
