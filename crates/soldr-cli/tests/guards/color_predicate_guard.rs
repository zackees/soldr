//! soldr#3437: `NO_COLOR` is read in exactly one file, and the ANSI palette is
//! declared in exactly one file.
//!
//! # Why a source scan
//!
//! The unit tests in `color_choice_tests.rs` prove the *rule* is right; they
//! cannot prove that every surface actually asks for it. Six hand-rolled
//! predicates coexisted for months precisely because each one looked correct
//! in isolation — the defect lived only *between* them, so no test of any one
//! of them could see it. A source scan is the only check with the same shape
//! as the defect.
//!
//! # The two rules
//!
//! 1. **`NO_COLOR` is read only by `color_choice.rs`.** A surface that reads
//!    it itself has re-founded a predicate, and the whole point of the
//!    unification is that there is one.
//! 2. **The escape palette (`GREEN` / `YELLOW` / `DIM` / `RESET`) is declared
//!    only by `color_choice.rs`.** The six sites each carried their own copy;
//!    a `const` outside the canonical module is the same duplication coming
//!    back through the constants rather than the predicate.
//!
//! Neither rule is an allowlist to maintain: rule 1 is a literal substring
//! that only a real read spells, and rule 2 only matches `const` initializers,
//! so assertions inside test modules (which compare against escape strings)
//! are not offences.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// The one file allowed to read `NO_COLOR`.
const CANONICAL: &str = "crates/soldr-cli/src/color_choice.rs";

/// Every workspace crate's `src/`, so a second crate cannot grow its own copy.
///
/// Resolved at *runtime*: `CARGO_MANIFEST_DIR` is baked in at compile time and
/// points at the machine that built the archive, so the pre-built test-archive
/// lanes would silently scan nothing (see `env_lock_lint.rs`).
fn crate_src_roots() -> Vec<PathBuf> {
    let crates_dir = common::workspace_root().join("crates");
    let Ok(entries) = fs::read_dir(&crates_dir) else {
        return Vec::new();
    };
    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("src"))
        .filter(|src| src.is_dir())
        .collect();
    roots.sort();
    roots
}

fn any_src_root_exists(roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| root.is_dir())
}

fn repo_relative(path: &Path) -> String {
    let root = common::workspace_root();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `target/` holds generated code; it is not ours to police.
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn sources() -> Vec<PathBuf> {
    let roots = crate_src_roots();
    let mut files = Vec::new();
    for root in &roots {
        collect_rs(root, &mut files);
    }
    files.sort();
    files
}

/// `NO_COLOR` may be read only by the canonical module.
#[test]
fn no_color_is_read_in_exactly_one_file() {
    let roots = crate_src_roots();
    if !any_src_root_exists(&roots) {
        // The pre-built test-archive lanes run away from the checkout.
        eprintln!("color_predicate_guard: skipping — no workspace crate sources present");
        return;
    }
    assert!(
        !sources().is_empty(),
        "walker found no sources; it is not reaching the workspace"
    );

    let mut offenders = Vec::new();
    for path in sources() {
        let relative = repo_relative(&path);
        if relative == CANONICAL {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        // Only a real read spells this. Doc prose saying "`NO_COLOR` is
        // honored" does not, which is why the substring includes the call.
        for marker in ["var_os(\"NO_COLOR\")", "var(\"NO_COLOR\")"] {
            if body.contains(marker) {
                offenders.push(format!("{relative}: {marker}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "soldr#3437 unified six divergent color predicates into one rule, and the \
         rule is only one rule if a single file interprets `NO_COLOR`. A surface \
         that reads it itself has re-founded a predicate, and the two will drift \
         again — that is exactly the defect the issue filed. Route the decision \
         through `crate::color_choice::stderr_enabled()` (or `enabled(..)` for a \
         non-stderr stream) instead. Found:\n{}",
        offenders.join("\n")
    );
}

/// The ANSI palette may be declared only by the canonical module.
#[test]
fn the_ansi_palette_is_declared_in_exactly_one_file() {
    let roots = crate_src_roots();
    if !any_src_root_exists(&roots) {
        eprintln!("color_predicate_guard: skipping — no workspace crate sources present");
        return;
    }

    let mut offenders = Vec::new();
    for path in sources() {
        let relative = repo_relative(&path);
        if relative == CANONICAL {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for (number, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("const ") && trimmed.contains("\\x1b[") {
                offenders.push(format!("{relative}:{}: {}", number + 1, trimmed.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the escape constants belong in `color_choice.rs` next to the rule that \
         decides when to use them; each surface carrying its own copy is the same \
         duplication soldr#3437 removed. Use `crate::color_choice::paint(..)` and \
         its `GREEN` / `YELLOW` / `DIM` / `RESET`. Found:\n{}",
        offenders.join("\n")
    );
}
