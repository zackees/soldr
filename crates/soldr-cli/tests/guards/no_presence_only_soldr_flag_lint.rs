//! soldr#3609: a `SOLDR_*` switch must be read through `crate::core::flag`.
//!
//! `std::env::var_os("SOLDR_X").is_some()` turns the switch ON for `X=0` and
//! `X=false`. The ban is on the *string-literal* spelling only: a named
//! constant (an internal marker, or a path-valued override whose presence is
//! the meaning) is legitimate and is what this lint steers callers toward.
//!
//! Scans every `crates/*/src/**/*.rs`, skipping `*tests.rs` and `tests/` dirs.

use std::fs;
use std::path::{Path, PathBuf};

use crate::common;

const NEEDLE: &str = "var_os(\"SOLDR_";

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

#[test]
fn soldr_switches_are_not_read_by_presence_alone() {
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
        for (index, line) in body.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(NEEDLE) && line.contains(".is_some()") {
                offenders.push(format!("{relative}:{}: {}", index + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "presence-only read of a SOLDR_* switch (`=0` would turn it ON). Use \
         `crate::core::flag` (soldr#3609), or a named marker/path constant:\n  {}",
        offenders.join("\n  ")
    );
}
