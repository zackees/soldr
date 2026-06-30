//! Regression test for soldr#1026 — the v0.7.65 release trap.
//!
//! A release PR bumps `Cargo.toml`'s `[workspace.package].version`,
//! `package.json`'s top-level `"version"`, and `Cargo.lock`'s
//! `soldr-cli` `version` in lockstep. If any of the three drifts —
//! historically `Cargo.lock` was the one a human contributor forgot —
//! the CI lanes that pass `--locked` fail immediately with
//!
//!     error: cannot update the lock file because --locked was passed
//!     to prevent this
//!
//! That kills the whole release matrix before any code compiles. The
//! v0.7.65 run (#28338586417) burned for ~10 minutes diagnosing this
//! on a Friday afternoon — exactly the failure shape this test
//! prevents.
//!
//! The test is a single integration binary (lives under `tests/` so it
//! gets its own process, no race with other lib tests touching env)
//! and runs on every `cargo test` invocation.

use std::fs;
use std::path::{Path, PathBuf};

use soldr_cli::timed_test;
use std::time::Duration;

/// Locate the soldr repo root from the CARGO_MANIFEST_DIR convention.
/// `CARGO_MANIFEST_DIR` points at `<repo>/crates/soldr-cli`; we want
/// `<repo>/`.
fn repo_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR set by cargo at test time");
    PathBuf::from(manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/soldr-cli must live two levels under repo root")
        .to_path_buf()
}

/// Extract `[workspace.package] version = "..."` from the repo's
/// root `Cargo.toml`. Hand-parsed (no `toml` crate dep here) because
/// the file shape is stable and we only need one field.
fn read_cargo_toml_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let mut in_section = false;
    for raw in s.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.package]";
            continue;
        }
        if in_section {
            if let Some(rest) = line.strip_prefix("version") {
                let rest = rest.trim_start_matches([' ', '=']);
                let rest = rest.trim();
                if let Some(stripped) = rest.strip_prefix('"') {
                    if let Some(end) = stripped.find('"') {
                        return stripped[..end].to_string();
                    }
                }
            }
        }
    }
    panic!("could not find [workspace.package].version in Cargo.toml");
}

/// Extract `name = "soldr-cli"` package's `version` from Cargo.lock.
/// Walks each `[[package]]` block until name matches, then reads the
/// adjacent `version = "..."` line. The lockfile shape is set by
/// cargo and stable across Rust 1.x.
fn read_cargo_lock_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");
    let mut current_name: Option<String> = None;
    for raw in s.lines() {
        let line = raw.trim();
        if line == "[[package]]" {
            current_name = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start_matches([' ', '=']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    current_name = Some(stripped[..end].to_string());
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            if current_name.as_deref() == Some("soldr-cli") {
                let rest = rest.trim_start_matches([' ', '=']).trim();
                if let Some(stripped) = rest.strip_prefix('"') {
                    if let Some(end) = stripped.find('"') {
                        return stripped[..end].to_string();
                    }
                }
            }
        }
    }
    panic!("could not find soldr-cli's version entry in Cargo.lock");
}

/// Extract the top-level `"version"` field from package.json. Same
/// rationale as the Cargo.toml parser: shape is stable, no need to
/// pull in a JSON crate.
fn read_package_json_version(root: &Path) -> String {
    let s = fs::read_to_string(root.join("package.json")).expect("read package.json");
    for raw in s.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("\"version\"") {
            let rest = rest.trim_start_matches([' ', ':']).trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                if let Some(end) = stripped.find('"') {
                    return stripped[..end].to_string();
                }
            }
        }
    }
    panic!("could not find top-level \"version\" in package.json");
}

timed_test!(
    cargo_toml_cargo_lock_and_package_json_share_one_version,
    Duration::from_secs(15),
    {
        let root = repo_root();
        let cargo_toml = read_cargo_toml_version(&root);
        let cargo_lock = read_cargo_lock_version(&root);
        let package_json = read_package_json_version(&root);

        assert_eq!(
            cargo_toml, cargo_lock,
            "soldr#1026 v0.7.65 trap: Cargo.toml says {cargo_toml}, Cargo.lock says \
             {cargo_lock}. CI runs `cargo build --locked` and will fail with `cannot update \
             the lock file` until you refresh Cargo.lock (run `cargo build -p soldr-cli` \
             after bumping Cargo.toml and `git add Cargo.lock`). See CLAUDE.md \
             'Bumping soldr's own version (release PRs)'."
        );
        assert_eq!(
            cargo_toml, package_json,
            "soldr#1026 lockstep drift: Cargo.toml says {cargo_toml}, package.json says \
             {package_json}. The release pipeline reads BOTH — the npm-side wrapper bumps off \
             package.json and the crate bumps off Cargo.toml. Update package.json's top-level \
             \"version\" to match."
        );
    }
);
