//! `timed_test!` must not come back (soldr#2493).
//!
//! This file is the inversion of the old `timed_test_lint.rs`, which walked
//! every workspace crate and failed the build when a *new* test used a bare
//! `#[test]` instead of the macro. The macro is gone, so the rule flips: a
//! bare `#[test]` is now correct everywhere, and any reappearance of
//! `timed_test` is the regression.
//!
//! Why a guard at all, when deleting the macro already makes `timed_test!(…)`
//! a compile error? Because the failure this protects against is not a stray
//! call site — it is someone reintroducing a hand-rolled per-test watchdog
//! because they did not know why this one was removed. The two reasons are
//! worth restating where a future author will hit them:
//!
//!   1. The diagnostic is invisible. A watchdog reports through `eprintln!`,
//!      libtest captures that, and a captured buffer is only printed when
//!      libtest reports a result — which `abort()` guarantees never happens.
//!      CI sees a bare `signal: 6, SIGABRT`.
//!   2. The blast radius is the whole binary. `abort()` takes down every other
//!      test in the process, including ones nowhere near their own budget.
//!
//! Per-test timeouts belong to cargo-nextest, configured in
//! `.config/nextest.toml` via `slow-timeout` / `terminate-after`. nextest runs
//! each test in its own process, so a timeout names one test, preserves its
//! output, and kills only that test.
//!
//! The walker is deliberately the same shape as the lint it replaces: every
//! `.rs` file under `crates/`, no allowlist to maintain.

use crate::common;

use std::path::{Path, PathBuf};

/// Two files are allowed to name the removed macro: this guard, and
/// `test_util.rs`, whose module doc explains why the macro is gone. That
/// explanation is the whole point of keeping a record — someone reading
/// `test_util.rs` and wondering where the watchdog went should find the answer
/// there rather than in a commit message.
/// Matched on the repo-relative path, not the bare file name: exempting every
/// file called `test_util.rs` anywhere under `crates/` would be a standing hole
/// in the guard.
const EXEMPT_PATHS: &[&str] = &[
    "crates/soldr-cli/tests/guards/no_timed_test_guard.rs",
    "crates/soldr-core/src/test_util.rs",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds generated code; it is not ours to police.
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn timed_test_is_not_reintroduced() {
    // `common::workspace_root()` resolves at *runtime*. `CARGO_MANIFEST_DIR`
    // would bake this machine's path into the binary, which breaks the
    // nextest-archive replay on the target-run lanes (they remap the workspace
    // on a different host). `test_archived_source_tests_use_only_runtime_
    // workspace_resolution` enforces that.
    let crates_dir = common::workspace_root().join("crates");
    let mut sources = Vec::new();
    rust_sources(&crates_dir, &mut sources);
    assert!(
        sources.len() > 100,
        "walker found only {} files; it is not reaching the workspace",
        sources.len()
    );

    let root = common::workspace_root();
    let mut offenders = Vec::new();
    for path in sources {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT_PATHS.contains(&relative.as_str()) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (number, line) in body.lines().enumerate() {
            // This guard's own name contains the banned token, and other lints
            // cross-reference it by name. Blank those mentions out first so a
            // pointer to the guard is not itself an offence.
            let line = line.replace("no_timed_test_guard", "");
            if line.contains("timed_test") {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "`timed_test` was removed in soldr#2493; per-test timeouts belong to \
         cargo-nextest's `slow-timeout` / `terminate-after` in \
         `.config/nextest.toml`. A per-test watchdog that calls `abort()` \
         reports through a libtest-captured buffer that is never printed, and \
         tears down every other test in the binary. Found:\n{}",
        offenders.join("\n")
    );
}
