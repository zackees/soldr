//! `soldr logs` — Phase 1 of the discoverable logs API tracked in
//! [issue #820](https://github.com/zackees/soldr/issues/820).
//!
//! This module currently implements only one verb — `paths` — which
//! prints every directory soldr writes session, lifecycle, and
//! daemon-spawn logs into. The follow-up verbs in the issue
//! (`list`, `show`, `view`, `prune`) ride on the same `Commands::Logs`
//! arm; this module is the foundation they'll extend.
//!
//! Goal: 15-minute-grep-for-the-right-journal becomes one command.
//! On a vanilla `~/.soldr/` install with no `SOLDR_CACHE_DIR`
//! override, the directories printed today are the same ones the
//! issue's repro mentioned (`logs/last-session.{log,jsonl,stats.json}`,
//! `daemon-{lifecycle,spawn}-*.log`, trash mailboxes, runtime state).
//! Future verbs will read them; this one just names them.
//!
//! ## JSON shape (stable contract)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "root": "/home/user/.soldr",
//!   "paths": [
//!     {
//!       "name": "soldr-root",
//!       "path": "/home/user/.soldr",
//!       "description": "User-level soldr install root...",
//!       "exists": true
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! `schema_version` bumps on a breaking change. Field additions are
//! additive (consumer code reading by name is forward-compatible).

use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Debug, Clone)]
pub(crate) struct LogPathEntry {
    /// Short stable identifier (snake_case). JSON consumers filter by
    /// this. Human output prefixes the line with `[name]`.
    pub name: String,
    pub path: PathBuf,
    pub description: String,
    pub exists: bool,
}

#[derive(Serialize, Debug)]
pub(crate) struct LogPathsOutput {
    pub schema_version: u32,
    pub root: PathBuf,
    pub paths: Vec<LogPathEntry>,
}

/// Implementation of `soldr logs paths`. Returns the soldr exit code:
/// always `0` on success — the command's job is informational, not
/// diagnostic. JSON mode emits a stable `schema_version: 1` payload.
pub(crate) fn run_logs_paths(json: bool) -> Result<i32, SoldrError> {
    let paths = SoldrPaths::new()?;
    let output = build_log_paths_output(&paths);

    if json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }
    Ok(0)
}

/// Pure-function constructor for the [`LogPathsOutput`] used by both
/// the JSON and human emit paths. Lets unit tests drive the shape
/// without doing filesystem I/O for emission.
pub(crate) fn build_log_paths_output(paths: &SoldrPaths) -> LogPathsOutput {
    let entries = collect_log_path_entries(&paths.root, &paths.cache, &paths.bin);
    LogPathsOutput {
        schema_version: SCHEMA_VERSION,
        root: paths.root.clone(),
        paths: entries,
    }
}

/// Walk the known list of directories soldr writes runtime logs into
/// and stamp each with an `exists` boolean based on the live
/// filesystem. The fixed list mirrors the issue #820 repro's
/// "non-obvious tour" and the directories `cache_lib` / `daemon` /
/// `gc` actually use.
fn collect_log_path_entries(root: &Path, cache: &Path, bin: &Path) -> Vec<LogPathEntry> {
    let zccache = cache.join("zccache");
    let zccache_private = zccache.join("private");
    let zccache_default_logs = zccache.join("logs");
    let runtime = root.join("runtime");
    let runtime_daemon = runtime.join("soldr-daemon");
    let runtime_self = runtime.join("soldr-self");

    let entries = [
        (
            "soldr-root",
            root.to_path_buf(),
            "User-level soldr install root (`~/.soldr/` unless `SOLDR_CACHE_DIR` overrides). \
             Every other log path is anchored under this.",
        ),
        (
            "soldr-bin",
            bin.to_path_buf(),
            "Managed binary install dir. Per-version subdirs (e.g. `zccache-<ver>/`, \
             `crgx-<ver>/`) hold the fetched tools soldr's resolver materialized.",
        ),
        (
            "zccache-cache-root",
            zccache.clone(),
            "Root of soldr's managed zccache cache. Contains both the default-session \
             `logs/` and per-private-daemon subtrees under `private/`.",
        ),
        (
            "zccache-default-session-logs",
            zccache_default_logs,
            "Default-session log directory. Used when soldr does NOT start a private \
             daemon. `soldr cache report --json` reads from here.",
        ),
        (
            "zccache-private-daemon-roots",
            zccache_private,
            "Per-project private-daemon namespaces (`soldr-dev-<hash>/`). Each holds a \
             `logs/` subdir with `last-session.{log,jsonl,stats.json}` overwritten on each \
             soldr session. This is where the issue #820 repro's slow-build journal lived.",
        ),
        (
            "soldr-daemon-runtime",
            runtime_daemon,
            "Per-version soldr-daemon runtime root. `daemon-lifecycle-*.log` + \
             `daemon-spawn-*.log` land here when the daemon auto-starts.",
        ),
        (
            "soldr-self-trampoline-dir",
            runtime_self,
            "Windows-only: relocated copies of `soldr.exe` made when soldr is run from a \
             disposable worktree. The trampoline (issue #1064 era) ensures `RUSTC_WRAPPER` \
             doesn't point at a doomed worktree path.",
        ),
    ];

    entries
        .into_iter()
        .map(|(name, path, description)| {
            let exists = path.exists();
            LogPathEntry {
                name: name.to_string(),
                path,
                description: description.to_string(),
                exists,
            }
        })
        .collect()
}

fn emit_json(output: &LogPathsOutput) -> Result<(), SoldrError> {
    let s = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("serialize logs paths JSON: {e}")))?;
    println!("{s}");
    Ok(())
}

fn emit_human(output: &LogPathsOutput) {
    println!("soldr logs paths — issue #820 phase 1");
    println!("schema_version: {}", output.schema_version);
    println!("root: {}", output.root.display());
    println!();
    for entry in &output.paths {
        let marker = if entry.exists { "✓" } else { "·" };
        println!("[{marker}] {}", entry.name);
        println!("    path: {}", entry.path.display());
        // Wrap the description at a soft 78 cols by walking spaces.
        for line in wrap_description(&entry.description, 74) {
            println!("    {line}");
        }
        println!();
    }
    println!("legend: ✓ = directory exists today | · = not yet created");
    println!("Run `soldr logs paths --json` for a machine-readable form.");
}

/// Hand-rolled greedy line wrapper so we don't pull in a textwrap dep
/// just for one info command. Splits on whitespace, packs words into
/// lines whose width is `<= width` (the first word always lands even
/// if it overflows alone — better one long line than dropped output).
fn wrap_description(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }
        if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;
    use std::time::Duration;

    timed_test!(
        build_log_paths_output_carries_schema_version_one,
        Duration::from_secs(5),
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
            let output = build_log_paths_output(&paths);
            assert_eq!(output.schema_version, 1);
            assert_eq!(output.root, tmp.path());
            assert!(!output.paths.is_empty(), "must include at least one entry");
        }
    );

    timed_test!(
        build_log_paths_output_names_the_private_daemon_root,
        Duration::from_secs(5),
        {
            // The issue #820 repro's hardest-to-find log lived under
            // `~/.soldr/cache/zccache/private/soldr-dev-<hash>/logs/`.
            // The entry MUST surface that root so the human/agent finds
            // the right journal in 30 seconds.
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
            let output = build_log_paths_output(&paths);
            let entry = output
                .paths
                .iter()
                .find(|e| e.name == "zccache-private-daemon-roots")
                .expect("private-daemon entry must exist");
            let expected = tmp.path().join("cache").join("zccache").join("private");
            assert_eq!(entry.path, expected);
            assert!(
                entry.description.contains("private-daemon"),
                "description should mention private-daemon, got: {}",
                entry.description
            );
        }
    );

    timed_test!(
        build_log_paths_output_names_soldr_daemon_runtime,
        Duration::from_secs(5),
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
            let output = build_log_paths_output(&paths);
            let entry = output
                .paths
                .iter()
                .find(|e| e.name == "soldr-daemon-runtime")
                .expect("soldr-daemon-runtime entry must exist");
            let expected = tmp.path().join("runtime").join("soldr-daemon");
            assert_eq!(entry.path, expected);
        }
    );

    timed_test!(
        build_log_paths_output_marks_missing_dirs,
        Duration::from_secs(5),
        {
            let tmp = tempfile::tempdir().expect("tmpdir");
            let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
            let output = build_log_paths_output(&paths);
            // Fresh tmpdir → no soldr install → most entries should be
            // `exists = false`. The root itself exists (it's the tmpdir).
            let root_entry = output
                .paths
                .iter()
                .find(|e| e.name == "soldr-root")
                .expect("soldr-root entry must exist");
            assert!(root_entry.exists, "soldr-root should exist (tmpdir)");
            let zccache_entry = output
                .paths
                .iter()
                .find(|e| e.name == "zccache-private-daemon-roots")
                .expect("private-daemon entry must exist");
            assert!(
                !zccache_entry.exists,
                "private-daemon dir under a fresh tmpdir must NOT exist yet"
            );
        }
    );

    timed_test!(
        wrap_description_handles_long_text,
        Duration::from_secs(5),
        {
            let lines = wrap_description("the quick brown fox jumps over the lazy dog", 12);
            // each line must be <= 12 chars (greedy fit; first word always
            // lands even if it overflows).
            for line in &lines {
                assert!(line.len() <= 12, "line too long: {line:?}");
            }
            // joined back must equal the original (modulo whitespace).
            let joined = lines.join(" ");
            assert_eq!(joined, "the quick brown fox jumps over the lazy dog");
        }
    );

    timed_test!(json_output_is_valid_json, Duration::from_secs(5), {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let output = build_log_paths_output(&paths);
        let s = serde_json::to_string(&output).expect("serialize");
        // Round-trip through serde_json::Value to confirm well-formed.
        let v: serde_json::Value = serde_json::from_str(&s).expect("re-parse");
        assert_eq!(v["schema_version"], serde_json::Value::from(1));
        let arr = v["paths"].as_array().expect("paths must be array");
        assert!(!arr.is_empty());
    });
}
