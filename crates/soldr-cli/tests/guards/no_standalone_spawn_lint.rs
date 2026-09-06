//! Regression guard for Soldr's embedded-only zccache contract.
//!
//! Soldr owns every user-facing cache command. Production code must not enter
//! an upstream zccache CLI dispatcher, its lazy standalone-daemon helpers, or
//! an environment-selected external zccache executable.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

const FORBIDDEN_ENTRY_POINTS: &[&str] = &[
    "zccache::cli::commands::run(",
    "zccache::cli::commands::run_with_args(",
    "zccache::cli::ensure_daemon",
    "zccache::cli::spawn_daemon",
    "zccache::cli::prepare_daemon_exe",
    "embedded_zccache_binary",
];

const FORBIDDEN_EXTERNAL_ZCCACHE_BINARY_ENV_VARS: &[&str] =
    &["SOLDR_TEST_ZCCACHE_BIN", "SOLDR_ZCCACHE_BIN"];

/// The one place `SOLDR_ZCCACHE_BIN` is allowed to appear as a literal.
///
/// It is a documented legacy compatibility name (docs/API.md) that Soldr
/// *scrubs* from child processes rather than honouring — `zccache.rs` calls
/// `env_remove` on it — so the constant has to be spelled somewhere. Widening
/// this lint to every workspace crate (soldr#2901) brought that declaration
/// into scope for the first time; allowing the declaration line, rather than
/// dropping the name from the list, keeps every *use* site still forbidden.
const ALLOWED_BINARY_ENV_VAR_DECLARATION: &str = "pub const ZCCACHE_BINARY_ENV_VAR: &str =";

/// soldr#2899 removed the last `zccache::cli::*` call site (the rustfmt
/// adapter now uses the daemon-free `zccache::formatter` API) and soldr#2901
/// dropped the `cli` feature, so the module no longer compiles into Soldr at
/// all. Naming the module prefix — not just the five entry points above —
/// makes a re-entry a source-lint failure rather than a link error at the
/// bottom of a CI log.
const FORBIDDEN_ZCCACHE_CLI_MODULE: &str = "zccache::cli";

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "target" || name == ".git" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn code_lines(body: &str) -> impl Iterator<Item = (usize, &str)> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
}

#[test]
fn soldr_never_enters_the_upstream_zccache_cli() {
    // soldr#2901: scan every workspace crate, not just soldr-cli. The three
    // crates that depended on zccache could each have re-entered the CLI.
    let crate_root = common::crate_root();
    let root = crate_root
        .parent()
        .expect("soldr-cli crate root lies under workspace crates/");
    let mut files = Vec::new();
    for crate_dir in fs::read_dir(root)
        .expect("read workspace crates directory")
        .flatten()
    {
        collect_rs_files(&crate_dir.path().join("src"), &mut files);
    }
    assert!(!files.is_empty(), "lint found no source files");

    let mut offenders = Vec::new();
    for file in files {
        let Ok(body) = fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(root)
            .expect("source file lies under the workspace crates directory")
            .to_string_lossy()
            .replace('\\', "/");
        for (index, line) in code_lines(&body) {
            for forbidden in FORBIDDEN_ENTRY_POINTS {
                if line.contains(forbidden) {
                    offenders.push(format!("{relative}:{}: references {forbidden}", index + 1));
                }
            }
            if line.contains(FORBIDDEN_ZCCACHE_CLI_MODULE) {
                offenders.push(format!(
                    "{relative}:{}: references {FORBIDDEN_ZCCACHE_CLI_MODULE}",
                    index + 1
                ));
            }
            if line.contains(ALLOWED_BINARY_ENV_VAR_DECLARATION) {
                continue;
            }
            for env_var in FORBIDDEN_EXTERNAL_ZCCACHE_BINARY_ENV_VARS {
                if line.contains(env_var) {
                    offenders.push(format!(
                        "{relative}:{}: references {env_var}, an external zccache executable override",
                        index + 1
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "embedded-only zccache contract violations:\n  {}",
        offenders.join("\n  ")
    );
}
