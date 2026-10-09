//! soldr#3462: `zccache::core::mtime` is the single owner of
//! materialized-output mtime policy.
//!
//! A compile output restored from a cache must get its mtime from
//! `zccache::core::mtime::{stamp_mtime, stamp_recorded_mtime}`, not from a
//! private `filetime` call, or the policy forks and cargo fingerprints drift
//! between restore paths. This scans every `crates/*/src/**/*.rs` (production
//! code only: `*tests.rs` files, `tests/` dirs and everything from the first
//! `#[cfg(test)]` line on are skipped) for file-time setters, and fails on any
//! file that is not in [`ALLOWLIST`].
//!
//! The allowlist is for writers that touch something other than a compile
//! output, or that must write through an open handle. Each entry states why.
//! Tar-header `set_mtime` calls (`save_archive.rs`,
//! `archive_cmd.rs`) write an archive header field, not a file time, and are
//! not matched. Mirrors zccache's own `tests/mtime_owner_workspace.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

/// Substrings that set a file time on disk.
const SETTERS: &[&str] = &[
    "set_file_mtime(",
    "set_file_times(",
    "set_file_handle_times(",
    "set_symlink_file_times(",
    ".set_modified(",
    ".set_times(",
];

/// `(path relative to `crates/`, reason it is not a materialized compile output)`.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "soldr-cli/src/cargo_front_door/no_cache_detach.rs",
        "preserves an mtime through a capability-relative open handle; \
         zccache's stamp_mtime takes a path, and re-opening by path would \
         reintroduce the symlink race the detach design closes",
    ),
    (
        "soldr-cli/src/shim_materialize.rs",
        "copies a tool-shim executable, not a compile output",
    ),
    (
        "soldr-daemon/src/daemon/service_definition.rs",
        "renews a daemon registration's liveness timestamp",
    ),
    (
        "soldr-nextest-wrapper/src/guard.rs",
        "stamps the resume-spacing marker between test starts",
    ),
];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "target" || name == "tests" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if name.ends_with(".rs") && !name.ends_with("tests.rs") {
            out.push(path);
        }
    }
}

/// Production lines of a file: everything before the first `#[cfg(test)]`.
fn setter_lines(body: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (index, line) in body.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        if SETTERS.iter().any(|setter| line.contains(setter)) {
            hits.push((index + 1, trimmed.to_string()));
        }
    }
    hits
}

#[test]
fn only_zccache_core_mtime_sets_output_mtimes() {
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
    assert!(!files.is_empty(), "guard found no source files");

    let mut offenders = Vec::new();
    let mut used_allowances = Vec::new();
    for file in files {
        let Ok(body) = fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(root)
            .expect("source file lies under the workspace crates directory")
            .to_string_lossy()
            .replace('\\', "/");
        let hits = setter_lines(&body);
        if hits.is_empty() {
            continue;
        }
        if ALLOWLIST.iter().any(|(path, _)| *path == relative) {
            used_allowances.push(relative);
            continue;
        }
        for (line, text) in hits {
            offenders.push(format!("{relative}:{line}: {text}"));
        }
    }

    assert!(
        offenders.is_empty(),
        "file-time write outside `zccache::core::mtime` (soldr#3462). Route a \
         compile-output mtime through `zccache::core::mtime::stamp_mtime` / \
         `stamp_recorded_mtime`; add an ALLOWLIST entry with a reason only for \
         a writer that is not a materialized output:\n  {}",
        offenders.join("\n  ")
    );

    for (path, reason) in ALLOWLIST {
        assert!(!reason.is_empty(), "allowlist entry {path} needs a reason");
        assert!(
            used_allowances.iter().any(|used| used == path),
            "stale allowlist entry {path}: it no longer sets a file time"
        );
    }
}
