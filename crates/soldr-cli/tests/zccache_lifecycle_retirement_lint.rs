//! Regression guard for removal of the retired private zccache lifecycle.
//!
//! Cacheable compiler work is brokered by Soldr and hosted in-process by
//! `soldr-daemon`; it must not grow a second lifecycle that selects or launches
//! a private zccache executable.

use std::fs;
use std::path::{Path, PathBuf};

mod common;

const RETIRED_LIFECYCLE_SYMBOLS: &[&str] = &[
    "ZccacheBuildSession",
    "zccache_lifecycle",
    "ZCCACHE_DAEMON_NAMESPACE_ENV_VAR",
    "run_zccache_command_strings_in_cache_dir_with_daemon_name",
    "private_zccache_cache_dir",
    "resolve_private_zccache_daemon_name",
];

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read source directory").flatten() {
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
    let root = common::crate_root();
    let mut files = Vec::new();
    collect_rust_files(&root.join("src"), &mut files);

    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .expect("source file under crate root")
            .display();
        let body = fs::read_to_string(&file).expect("read source file");
        for symbol in RETIRED_LIFECYCLE_SYMBOLS {
            if body.contains(symbol) {
                offenders.push(format!("{}: {symbol}", relative));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "retired private zccache lifecycle references found:\n  {}",
        offenders.join("\n  ")
    );
}
