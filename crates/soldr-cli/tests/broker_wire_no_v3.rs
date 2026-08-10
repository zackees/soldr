//! soldr#2361 cross-cutting invariant #5 — **No v3.** The running-process
//! broker wire stays `protocol_v2` / `client_v2`; breaking changes are made in
//! place on v2 and enforced by the min-version floor, never by minting a new
//! protocol generation. This guard fails the build if a `protocol_v3` /
//! `client_v3` symbol is ever introduced, so the "break v2 in place" policy is
//! enforced by a test rather than by convention.
//!
//! It walks soldr's own crates AND the vendored broker (`_vender/running-process`)
//! — the two places a v3 generation could appear — and greps for the forbidden
//! identifiers as whole tokens (so unrelated substrings like a `..._v30`
//! version string or a comment describing the policy do not trip it).

mod common;

use std::path::{Path, PathBuf};

use soldr_cli::timed_test;

/// Forbidden identifiers (as whole `snake_case` tokens). A v3 broker wire would
/// surface as one of these module/type names.
const FORBIDDEN: &[&str] = &["protocol_v3", "client_v3"];

fn repo_root() -> PathBuf {
    // tests run with CWD = the crate dir (crates/soldr-cli); walk up to the repo.
    std::env::current_dir()
        .expect("cwd")
        .ancestors()
        .find(|c| c.join("Cargo.toml").is_file() && c.join("crates").is_dir())
        .expect("find repo root")
        .to_path_buf()
}

fn contains_forbidden_token(text: &str) -> Option<&'static str> {
    for needle in FORBIDDEN {
        // Whole-token match: the char before/after the hit must not be part of a
        // longer identifier, so `protocol_v3` matches but `protocol_v30` /
        // `my_protocol_v3x` do not.
        let bytes = text.as_bytes();
        let mut from = 0;
        while let Some(rel) = text[from..].find(needle) {
            let start = from + rel;
            let end = start + needle.len();
            let before_ok = start == 0 || !is_ident_char(bytes[start - 1]);
            let after_ok = end >= bytes.len() || !is_ident_char(bytes[end]);
            if before_ok && after_ok {
                return Some(needle);
            }
            from = start + 1;
        }
    }
    None
}

fn is_ident_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn scan_rust_sources(dir: &Path, hits: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip build output — it can contain generated copies of source.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan_rust_sources(&path, hits);
        } else if path.extension().is_some_and(|e| e == "rs") {
            // Skip this guard file itself — it names the forbidden tokens on
            // purpose.
            if path
                .file_name()
                .is_some_and(|n| n == "broker_wire_no_v3.rs")
            {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(tok) = contains_forbidden_token(&text) {
                    hits.push(format!("{}: contains `{tok}`", path.display()));
                }
            }
        }
    }
}

timed_test!(no_broker_protocol_or_client_v3_symbol_exists, {
    let root = repo_root();
    let mut hits = Vec::new();
    scan_rust_sources(&root.join("crates"), &mut hits);
    let vendored = root.join("_vender").join("running-process");
    if vendored.is_dir() {
        scan_rust_sources(&vendored, &mut hits);
    }
    assert!(
        hits.is_empty(),
        "soldr#2361 invariant #5 (No v3): the broker wire stays v2, broken in \
         place — no `protocol_v3` / `client_v3` symbol may exist. Found:\n  {}",
        hits.join("\n  ")
    );
});
