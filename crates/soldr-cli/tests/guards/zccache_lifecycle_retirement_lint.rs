//! Regression guard for removal of the retired private zccache lifecycle
//! (soldr#2900).
//!
//! Cacheable compiler work is brokered by Soldr and hosted in-process by
//! `soldr-daemon`; it must not grow a second lifecycle that selects, names,
//! or launches a private zccache executable. The retired module carried a
//! daemon-namespace env var, a `private/<daemon-name>` cache-dir layout, and
//! a `Command` runner — none of which any live code path reaches.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// Symbols that existed only to support the pre-embedded private-zccache
/// lifecycle. Any reappearance under a workspace crate's `src/` means the
/// retired layer is being rebuilt.
const RETIRED_LIFECYCLE_SYMBOLS: &[&str] = &[
    "ZccacheBuildSession",
    "ZccachePrivateEnv",
    "zccache_lifecycle",
    "ZCCACHE_DAEMON_NAMESPACE",
    "run_zccache_command_strings_in_cache_dir_with_daemon_name",
    "private_zccache_cache_dir",
    "resolve_private_zccache_daemon_name",
    "sanitize_zccache_daemon_name",
];

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn retired_private_zccache_lifecycle_is_absent_from_production_source() {
    let cli_root = common::crate_root();
    let crates_root = cli_root
        .parent()
        .expect("soldr-cli crate must live under workspace crates/");
    let mut files = Vec::new();
    for crate_dir in fs::read_dir(crates_root)
        .expect("read workspace crates directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
    {
        let source_dir = crate_dir.join("src");
        if source_dir.is_dir() {
            collect_rust_files(&source_dir, &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "lint scanned only {} source files under {}; the crate layout moved",
        files.len(),
        crates_root.display()
    );

    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(crates_root)
            .expect("source file under workspace crates directory")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(body) = fs::read_to_string(&file) else {
            continue;
        };
        for (index, line) in body.lines().enumerate() {
            for symbol in RETIRED_LIFECYCLE_SYMBOLS {
                if line.contains(symbol) {
                    offenders.push(format!("{relative}:{}: {symbol}", index + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "retired private zccache lifecycle references found:\n  {}",
        offenders.join("\n  ")
    );
}
