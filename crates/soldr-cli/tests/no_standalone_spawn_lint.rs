//! Regression guard for the no-standalone-zccache-daemon contract
//! (soldr#1467).
//!
//! soldr's build cache runs as an embedded service inside soldr-daemon
//! (`Request::Compile`); a standalone `zccache-daemon` process must
//! never spawn. The compiled-in `zccache` trampoline
//! (`src/zccache_entry.rs`) is the single gated entry into the
//! vendored zccache CLI. This lint walks every `.rs` file under
//! `crates/soldr-cli/src/` (like `tests/timed_test_lint.rs`) and
//! asserts the source-level invariants that keep the contract:
//!
//! 1. Only `src/zccache_entry.rs` may call
//!    `zccache::cli::commands::run` — the gated trampoline is the sole
//!    CLI entry point.
//! 2. `src/zccache_entry.rs` keeps its gate: it must reference
//!    both `ZCCACHE_NO_SPAWN` (the zccache#982 defense-in-depth env
//!    guard) and `ALLOWED_SUBCOMMANDS` (the daemon-free allowlist).
//! 3. No file under `src/` references the upstream lazy-spawn entry
//!    points `zccache::cli::ensure_daemon`, `zccache::cli::spawn_daemon`,
//!    or `zccache::cli::prepare_daemon_exe`.
//!
//! Comment lines (`//`, `//!`, `///`) are exempt — docs may *mention*
//! the entry points; only code may not call them.

use std::fs;
use std::path::{Path, PathBuf};

use soldr_cli::timed_test;

mod common;

/// The one file allowed to reference `zccache::cli::commands::run`.
const TRAMPOLINE: &str = "src/zccache_entry.rs";

/// Upstream zccache CLI entry points that lazily spawn (or stage the
/// binary for) a standalone `zccache-daemon`. Referencing any of these
/// from soldr source re-opens the spawn path that soldr#1467 closed.
const FORBIDDEN_SPAWN_ENTRY_POINTS: &[&str] = &[
    "zccache::cli::ensure_daemon",
    "zccache::cli::spawn_daemon",
    "zccache::cli::prepare_daemon_exe",
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively collect `.rs` files under `dir`, skipping build output
/// and hidden directories (same shape as `tests/timed_test_lint.rs`).
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == "target" || name == ".git" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Render a file path relative to the crate root with forward slashes
/// so findings are stable across Windows and Unix.
fn relative_unix_path(abs: &Path, root: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Code lines only: strip `//`-style comment lines so documentation may
/// mention the forbidden entry points without tripping the lint. A
/// textual scan (no parser) matches the sibling `timed_test_lint.rs`
/// approach — zero extra dev-dependencies.
fn code_lines(body: &str) -> impl Iterator<Item = (usize, &str)> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
}

timed_test!(
    zccache_cli_run_is_only_referenced_by_the_gated_trampoline,
    {
        // soldr#1040 phase 2: source-tree-coupled — skip on cross-build
        // target runners that replay a nextest archive without src/.
        if common::should_skip_source_tree_test(
            "zccache_cli_run_is_only_referenced_by_the_gated_trampoline",
        ) {
            return;
        }
        let root = crate_root();
        let mut files = Vec::new();
        collect_rs_files(&root.join("src"), &mut files);
        assert!(
            !files.is_empty(),
            "lint found no source files under {}",
            root.join("src").display()
        );

        let mut offenders: Vec<String> = Vec::new();
        let mut trampoline_seen = false;

        for file in &files {
            let Some(rel) = relative_unix_path(file, &root) else {
                continue;
            };
            let Ok(body) = fs::read_to_string(file) else {
                continue;
            };

            if rel == TRAMPOLINE {
                trampoline_seen = true;
                // Invariant 2: the gate must stay in place.
                assert!(
                    body.contains("ZCCACHE_NO_SPAWN"),
                    "{TRAMPOLINE} must export the ZCCACHE_NO_SPAWN guard (zccache#982, soldr#1467)"
                );
                assert!(
                    body.contains("ALLOWED_SUBCOMMANDS"),
                    "{TRAMPOLINE} must gate on ALLOWED_SUBCOMMANDS (soldr#1467)"
                );
            }

            for (idx, line) in code_lines(&body) {
                // Invariant 1: only the trampoline calls the CLI entry point.
                if line.contains("zccache::cli::commands::run") && rel != TRAMPOLINE {
                    offenders.push(format!(
                        "{rel}:{}: references zccache::cli::commands::run (only {TRAMPOLINE} may)",
                        idx + 1
                    ));
                }
                // Invariant 3: the legacy spawn entry points stay dead.
                for pattern in FORBIDDEN_SPAWN_ENTRY_POINTS {
                    if line.contains(pattern) {
                        offenders.push(format!(
                            "{rel}:{}: references {pattern} (standalone-daemon spawn path, \
                         removed by soldr#1467)",
                            idx + 1
                        ));
                    }
                }
            }
        }

        assert!(
            trampoline_seen,
            "{TRAMPOLINE} not found — the gated zccache trampoline must exist"
        );

        assert!(
            offenders.is_empty(),
            "no-standalone-spawn contract violations (soldr#1467) — the build cache is an \
         embedded service inside soldr-daemon and nothing outside the gated trampoline \
         may reach the zccache CLI / daemon-spawn surface:\n  {}",
            offenders.join("\n  ")
        );
    }
);
