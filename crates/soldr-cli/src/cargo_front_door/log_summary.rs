//! soldr#1813 — print the session's log-file paths after every compilation.
//!
//! soldr writes a lot of log files per build session but never told the user
//! where they landed: you had to already know about `soldr logs paths` (#820)
//! or go spelunking under `~/.soldr`. This module renders a short summary that
//! the cargo front door prints at its tail, on both the success and the
//! failure path.
//!
//! Scope: **only logs this session actually wrote**. The full inventory stays
//! behind `soldr logs paths`, which the summary's closing line points at. That
//! keeps the output to a handful of lines on every build rather than a wall of
//! mostly-nonexistent paths.
//!
//! Lives in its own file so `cargo_front_door/mod.rs` doesn't grow further
//! (house style, post-#339).

use std::path::PathBuf;

use crate::daemon::protocol::BuildLogPaths;

/// Opt-out, following the `SOLDR_NO_*` convention (`SOLDR_NO_TRAMPOLINE`).
pub(super) const NO_LOG_SUMMARY_ENV_VAR: &str = "SOLDR_NO_LOG_SUMMARY";

/// Command that lists the log locations this summary deliberately omits.
const FULL_INVENTORY_HINT: &str = "soldr logs paths";

const HEADER: &str = "soldr: logs for this build session:";

/// ANSI dim, applied to the paths only when the sink is a color-capable TTY.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The log files a single build session wrote.
///
/// Every field is `Option` because each log is independently best-effort — a
/// build with the cache disabled has no `BuildLogPaths`, a build with no
/// fallbacks has no fallback log, and any individual write can fail.
#[derive(Debug, Default)]
pub(super) struct SessionLogs {
    /// The always-on per-build XML log (`<root>/logs/builds/<...>.xml`,
    /// soldr#1790). Present whenever `write_build_log` succeeded.
    pub(super) build_log: Option<PathBuf>,
    /// Live + archived zccache paths as recorded on the build record.
    pub(super) build_log_paths: Option<BuildLogPaths>,
    /// `<root>/logs/compile-daemon-fallbacks.jsonl`, and only when *this*
    /// session appended to it — an untouched fallback log is not a log this
    /// build wrote.
    pub(super) compile_fallback_log: Option<PathBuf>,
}

impl SessionLogs {
    /// Labelled paths in print order.
    ///
    /// Labels intentionally echo the vocabulary of `soldr logs paths`
    /// (`logs_cmd::collect_log_path_entries`) so the two surfaces name the same
    /// file the same way.
    fn entries(&self) -> Vec<(&'static str, String)> {
        let mut entries: Vec<(&'static str, String)> = Vec::new();
        if let Some(path) = &self.build_log {
            entries.push(("build log", path.display().to_string()));
        }
        if let Some(log_paths) = &self.build_log_paths {
            // Destructured exhaustively on purpose: adding a field to
            // `BuildLogPaths` then becomes a compile error here until someone
            // decides whether it is a log path worth printing. A wildcard arm
            // would let a new log silently stay invisible, which is the exact
            // bug this module exists to fix.
            let BuildLogPaths {
                // Not log files — correlation ids and directories.
                zccache_session_id: _,
                cache_dir: _,
                private_daemon_name: _,
                session_log_path,
                journal_path,
                session_stats_path,
                compile_journal_path,
                archived_session_log_path,
                archived_journal_path,
                archived_session_stats_path,
                archived_compile_journal_path,
            } = log_paths;
            for (label, value) in [
                ("zccache session log", session_log_path),
                ("zccache session journal", journal_path),
                ("zccache session stats", session_stats_path),
                ("compile journal", compile_journal_path),
                ("archived session log", archived_session_log_path),
                ("archived session journal", archived_journal_path),
                ("archived session stats", archived_session_stats_path),
                ("archived compile journal", archived_compile_journal_path),
            ] {
                if let Some(value) = value {
                    entries.push((label, value.clone()));
                }
            }
        }
        if let Some(path) = &self.compile_fallback_log {
            entries.push(("compile-daemon fallbacks", path.display().to_string()));
        }
        entries
    }
}

/// Render the summary, or `None` when this session wrote no logs at all.
///
/// Pure so it can be asserted directly in unit tests without capturing stderr
/// (same split as `compile_fallback_summary_message` / `emit_*`).
pub(super) fn summary_message(logs: &SessionLogs, use_color: bool) -> Option<String> {
    let entries = logs.entries();
    if entries.is_empty() {
        return None;
    }
    let label_width = entries
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    let mut out = String::from(HEADER);
    for (label, path) in entries {
        out.push_str("\n  ");
        out.push_str(label);
        for _ in label.len()..label_width {
            out.push(' ');
        }
        out.push_str("  ");
        if use_color {
            out.push_str(DIM);
            out.push_str(&path);
            out.push_str(RESET);
        } else {
            out.push_str(&path);
        }
    }
    out.push_str("\n  (all log locations: ");
    out.push_str(FULL_INVENTORY_HINT);
    out.push(')');
    Some(out)
}

/// Whether the block is worth printing for this exit.
///
/// A failing build is exactly when a reader needs the paths, so it always
/// prints. A green build on a terminal keeps the block too -- it is how an
/// interactive user learns where the logs went. A green build whose stderr
/// is a pipe (CI, `2>file`, an orchestrator such as `soldr ci-test` or the
/// Dylint cook running dozens of nested `soldr cargo` calls) prints nothing:
/// there the block repeated once per nested invocation and buried the
/// output that mattered, while the logs it points at are collected by the
/// orchestrator anyway.
pub(super) fn summary_wanted(exit_code: i32, stderr_is_terminal: bool) -> bool {
    exit_code != 0 || stderr_is_terminal
}

/// Print the summary to stderr unless suppressed.
pub(super) fn emit_session_log_summary(logs: &SessionLogs, exit_code: i32) {
    // soldr#2024: reaching here means Cargo ran and owned the terminal, so
    // this exit is accounted for even when the summary itself is suppressed.
    crate::exit_guard::mark_spoke();
    if super::env_flag_truthy(NO_LOG_SUMMARY_ENV_VAR) {
        return;
    }
    if !summary_wanted(
        exit_code,
        std::io::IsTerminal::is_terminal(&std::io::stderr()),
    ) {
        return;
    }
    if let Some(message) = summary_message(logs, use_color()) {
        eprintln!("{message}");
    }
}

/// Same gate as `emit_zthreads_fallback_warning`: never colorize for GitHub
/// Actions, an explicit `NO_COLOR`, or a non-TTY stderr.
fn use_color() -> bool {
    use std::io::IsTerminal;

    !super::foreign_env_flag("GITHUB_ACTIONS")
        && std::env::var_os("NO_COLOR").is_none()
        && std::io::stderr().is_terminal()
}
