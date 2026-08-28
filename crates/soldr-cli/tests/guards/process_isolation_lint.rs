//! Guard against test prose that assumes Cargo gives every source file a
//! dedicated process (soldr#2960).
//!
//! soldr#2934 consolidates the integration suite into category binaries.
//! Process-global fixtures must therefore be instance-scoped or explicitly
//! serialized; a runner choice cannot restore a layout that no longer exists.
//! This is intentionally a source lint: the dangerous premise lives in an
//! explanatory comment and can appear to work under a particular runner.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// These phrases make a correctness claim about a per-file executable or
/// dedicated process. Keep them narrower than generic binary-layout prose:
/// category `main.rs` files accurately describe one linked binary per category.
const PROHIBITED_CLAIMS: &[&str] = &[
    "its own test binary",
    "its own cargo-generated test binary",
    "its own integration-test binary",
    "a process to itself",
    "needs a process in which nothing else",
    "one binary per file",
    "one binary per source file",
    "one test binary per file",
    "one test binary per source file",
    "one process per file",
];

fn collect_rust_sources(dir: &Path, sources: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

fn prohibited_claim(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    PROHIBITED_CLAIMS
        .iter()
        .copied()
        .find(|claim| lower.contains(claim))
}

fn comment_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("//")
        .map(|comment| comment.trim_start_matches('/').trim_start())
}

#[test]
fn rejects_per_file_process_isolation_claims() {
    for claim in [
        "this test needs its own test binary",
        "this fixture requires its own cargo-generated test binary",
        "the test needs a process to itself",
        "this needs a process in which nothing else runs",
        "Cargo used to create one binary per file",
        "Cargo used to create one test binary per source file",
        "the old layout used one process per file",
    ] {
        assert!(
            prohibited_claim(claim).is_some(),
            "fixture must be rejected: {claim}"
        );
    }
}

#[test]
fn permits_factual_category_binary_prose() {
    for factual in [
        "soldr#2934: one linked test binary per category",
        "each category main.rs links its sibling test modules",
        "the daemon category binary contains this module",
    ] {
        assert!(
            prohibited_claim(factual).is_none(),
            "factual layout prose must remain allowed: {factual}"
        );
    }
}

#[test]
fn integration_tests_do_not_claim_per_file_process_isolation() {
    // Resolve at runtime: test archives execute away from a checkout, where
    // source-only guards have no source to inspect and must not manufacture a
    // failure from the build machine's embedded path.
    let root = common::workspace_root().join("crates/soldr-cli/tests");
    if !root.is_dir() {
        eprintln!(
            "process_isolation_lint: skipping â€” integration-test sources absent at {}",
            root.display()
        );
        return;
    }

    let mut sources = Vec::new();
    collect_rust_sources(&root, &mut sources);
    sources.sort();
    assert!(
        sources.len() > 50,
        "process-isolation guard found only {} integration-test sources",
        sources.len()
    );

    let workspace = common::workspace_root();
    let mut offenders = Vec::new();
    for source in sources {
        let relative = source
            .strip_prefix(&workspace)
            .unwrap_or(&source)
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(&source)
            .unwrap_or_else(|error| panic!("read {}: {error}", source.display()));
        for (index, line) in text.lines().enumerate() {
            let Some(comment) = comment_text(line) else {
                continue;
            };
            if let Some(claim) = prohibited_claim(comment) {
                offenders.push(format!("{relative}:{}: {claim}", index + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "soldr#2960: #2934 consolidated integration tests into category binaries, \
         so no test may claim per-file-binary or dedicated-process isolation. \
         Use an instance-scoped seam, parameter, or local fixture instead.\n  {}",
        offenders.join("\n  ")
    );
}
