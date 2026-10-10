//! Docs guard for soldr-daemon environment variables (soldr#3648).
//!
//! Every non-test `const NAME: &str = "SOLDR_*";` declared in
//! `crates/soldr-daemon/src` must be mentioned somewhere under `docs/`, so
//! daemon settings such as `SOLDR_SHUTDOWN_WATCHDOG_SECS` never ship
//! undocumented.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn collect_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read dir {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_files(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with("_tests.rs")
        || path
            .components()
            .any(|c| c.as_os_str() == std::ffi::OsStr::new("tests"))
}

/// Returns the first `"SOLDR_[A-Z0-9_]*"` literal on the line, if any.
fn env_literal(line: &str) -> Option<&str> {
    let mut rest = line;
    while let Some(pos) = rest.find("\"SOLDR_") {
        let body = &rest[pos + 1..];
        let len = body
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(body.len());
        if body[len..].starts_with('"') {
            return Some(&body[..len]);
        }
        rest = &rest[pos + 1..];
    }
    None
}

fn scan(text: &str, names: &mut BTreeSet<String>) {
    let mut pending_decl = false;
    for line in text.lines() {
        if line.trim().starts_with("#[cfg(test)]") {
            break;
        }
        let is_decl = line.contains("const ") && line.contains("&str");
        if is_decl || pending_decl {
            if let Some(name) = env_literal(line) {
                if !name.starts_with("SOLDR_TEST_") {
                    names.insert(name.to_string());
                }
                pending_decl = false;
                continue;
            }
        }
        pending_decl = is_decl && line.contains("&str =") && !line.contains('"');
    }
}

#[test]
fn every_daemon_soldr_env_const_is_documented() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    collect_files(&root.join("crates/soldr-daemon/src"), "rs", &mut sources);
    let mut names = BTreeSet::new();
    for path in sources.iter().filter(|p| !is_test_file(p)) {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        scan(&text, &mut names);
    }
    assert!(!names.is_empty(), "scanner found no SOLDR_* constants");
    assert!(
        names.contains("SOLDR_SHUTDOWN_WATCHDOG_SECS"),
        "scanner sanity check failed; found: {names:?}"
    );

    let mut docs = Vec::new();
    collect_files(&root.join("docs"), "md", &mut docs);
    let mut corpus = String::new();
    for path in &docs {
        corpus.push_str(&std::fs::read_to_string(path).unwrap_or_default());
        corpus.push('\n');
    }
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !corpus.contains(n.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "undocumented soldr-daemon env vars: {missing:?}\n\
         document it in docs/API.md Environment Variables (and docs/DAEMON_TIMEOUTS.md if it is a timeout) -- soldr#3648"
    );
}
