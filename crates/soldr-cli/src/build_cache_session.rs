//! Soldr-owned per-build cache metadata.
//!
//! This is intentionally just local bookkeeping for the embedded cache
//! service. It neither selects nor starts an external zccache process.

use std::process::Output;

pub(crate) const ANALYZE_NOTE_LIMIT: usize = 1000;

#[derive(Debug, Clone)]
pub(crate) struct BuildCacheSession {
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_log_path: std::path::PathBuf,
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) session_stats_path: std::path::PathBuf,
}

pub(crate) fn command_stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

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
