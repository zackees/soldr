//! Soldr-owned per-build cache metadata.
//!
//! This is intentionally just local bookkeeping for the embedded cache
//! service. It neither selects nor starts an external zccache process.
//!
//! soldr#2900 retired the private-zccache lifecycle module, which still
//! carried pre-embedded subprocess plumbing (a daemon namespace
//! env var, a private cache-dir layout, and a `Command` runner). Nothing
//! spawns a private zccache any more — cacheable compiler work is brokered
//! by Soldr and hosted in-process by `soldr-daemon` — so what survives here
//! is the session carrier plus two output formatters.

use std::process::Output;

pub(crate) const ANALYZE_NOTE_LIMIT: usize = 1000;

/// Correlation handle for one front-door build.
///
/// The paths are where the embedded cache service writes this build's
/// session log, compile journal, and stats. Nothing in here selects a
/// cache backend or launches a process.
#[derive(Debug, Clone)]
pub(crate) struct BuildCacheSession {
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_log_path: std::path::PathBuf,
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) session_stats_path: std::path::PathBuf,
}

/// Render a failed child process's stderr, falling back to its exit status
/// when it wrote nothing.
pub(crate) fn command_stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

/// Collapse captured process output into a single-line note, truncated to
/// [`ANALYZE_NOTE_LIMIT`] characters with a trailing ellipsis.
pub(crate) fn output_snippet(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut compact = String::new();
    let mut previous_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                compact.push(' ');
                previous_was_space = true;
            }
        } else {
            compact.push(ch);
            previous_was_space = false;
        }
    }

    let mut chars = compact.chars();
    let mut snippet: String = chars.by_ref().take(ANALYZE_NOTE_LIMIT).collect();
    if chars.next().is_some() {
        snippet.push_str("...");
    }
    Some(snippet)
}
