//! `soldr broker purge` (soldr#3193): stop every soldr-broker / soldr-daemon
//! process on this host that serves a `HOME` other than the current one.
//!
//! The inventory comes from `broker_inventory`; this module is the action.
//! Lives apart from `broker_cmd.rs` to keep that file under the production
//! line ceiling.

use std::time::{Duration, Instant};

use crate::core::SoldrError;

/// How long `purge` waits for a process to honour its terminate request
/// before force-killing it. Brokers exit on SIGTERM within milliseconds; the
/// budget is generous so a daemon flushing a cache is not killed mid-write.
const PURGE_DRAIN_DEADLINE: Duration = Duration::from_secs(5);
const PURGE_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum PurgeOutcome {
    /// `--dry-run`: listed, not signalled.
    WouldStop,
    /// Exited after the terminate request.
    Terminated,
    /// Ignored the terminate request and was force-killed.
    Forced,
    /// Gone before any signal was sent, or the PID no longer names a soldr
    /// process (reused since the scan): nothing was signalled.
    AlreadyGone,
    /// The signal itself failed (permissions: another user's process).
    Failed,
}

#[derive(Debug, Clone, serde::Serialize)]
struct PurgeRow {
    #[serde(flatten)]
    process: crate::broker_inventory::LeakedProcess,
    outcome: PurgeOutcome,
}

#[derive(Debug, serde::Serialize)]
struct PurgeReport {
    schema_version: u32,
    dry_run: bool,
    own_home: String,
    stopped: usize,
    failed: usize,
    processes: Vec<PurgeRow>,
}

/// Terminate one leaked process, verifying at each step that the PID still
/// names a soldr image so PID reuse can only ever make purge do nothing.
fn purge_one(process: &crate::broker_inventory::LeakedProcess) -> PurgeOutcome {
    use crate::broker_inventory::still_soldr_process;
    use crate::platform::process::terminate::signal_pid;

    if !still_soldr_process(process.pid, process.role) {
        return PurgeOutcome::AlreadyGone;
    }
    if signal_pid(process.pid, false).is_err() {
        return PurgeOutcome::Failed;
    }
    let deadline = Instant::now() + PURGE_DRAIN_DEADLINE;
    while Instant::now() < deadline {
        if !still_soldr_process(process.pid, process.role) {
            return PurgeOutcome::Terminated;
        }
        std::thread::sleep(PURGE_POLL);
    }
    if !still_soldr_process(process.pid, process.role) {
        return PurgeOutcome::Terminated;
    }
    match signal_pid(process.pid, true) {
        Ok(()) => PurgeOutcome::Forced,
        Err(_) => PurgeOutcome::Failed,
    }
}

pub(crate) fn run_broker_purge(dry_run: bool, json: bool) -> Result<(), SoldrError> {
    let Some(inventory) = crate::broker_inventory::scan() else {
        return Err(SoldrError::Other(
            "soldr broker purge: HOME is not set, so there is no own broker to spare".to_string(),
        ));
    };
    let processes: Vec<PurgeRow> = inventory
        .leaked
        .iter()
        .map(|process| PurgeRow {
            process: process.clone(),
            outcome: if dry_run {
                PurgeOutcome::WouldStop
            } else {
                purge_one(process)
            },
        })
        .collect();
    let stopped = processes
        .iter()
        .filter(|row| matches!(row.outcome, PurgeOutcome::Terminated | PurgeOutcome::Forced))
        .count();
    let failed = processes
        .iter()
        .filter(|row| row.outcome == PurgeOutcome::Failed)
        .count();
    let report = PurgeReport {
        schema_version: 1,
        dry_run,
        own_home: inventory.own_home.clone(),
        stopped,
        failed,
        processes,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| SoldrError::Other(error.to_string()))?
        );
    } else {
        print_purge_human(&report);
    }
    if failed > 0 {
        return Err(SoldrError::Other(format!(
            "soldr broker purge: {failed} process(es) could not be signalled"
        )));
    }
    Ok(())
}

fn print_purge_human(report: &PurgeReport) {
    if report.processes.is_empty() {
        println!(
            "soldr broker purge: no soldr processes for other HOMEs (own HOME {})",
            report.own_home
        );
        return;
    }
    let verb = if report.dry_run {
        "would stop"
    } else {
        "stopped"
    };
    println!(
        "soldr broker purge: {verb} {} of {} soldr process(es) serving HOMEs other than {}",
        if report.dry_run {
            report.processes.len()
        } else {
            report.stopped
        },
        report.processes.len(),
        report.own_home
    );
    for row in &report.processes {
        let outcome = match row.outcome {
            PurgeOutcome::WouldStop => "would stop",
            PurgeOutcome::Terminated => "terminated",
            PurgeOutcome::Forced => "forced",
            PurgeOutcome::AlreadyGone => "already gone",
            PurgeOutcome::Failed => "FAILED",
        };
        println!(
            "  {:<12} {:<7} pid {:<8} HOME {}{}",
            outcome,
            row.process.role.as_str(),
            row.process.pid,
            row.process.home.as_deref().unwrap_or("(unknown)"),
            if row.process.home_present {
                ""
            } else {
                "  [missing]"
            }
        );
    }
}
