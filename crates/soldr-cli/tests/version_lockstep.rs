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

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use soldr_cli::timed_test;
use std::time::Duration;

mod common;

/// Locate the soldr repo root from the CARGO_MANIFEST_DIR convention.
/// `CARGO_MANIFEST_DIR` points at `<repo>/crates/soldr-cli`; we want
/// `<repo>/`.
fn repo_root() -> PathBuf {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo at test time");
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

fn read_section_lines(manifest: &Path, section: &str) -> Vec<String> {
    let s = fs::read_to_string(manifest).unwrap_or_else(|e| {
        panic!("read {}: {e}", manifest.display());
    });
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut lines = Vec::new();
    for raw in s.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_section = line == header;
            continue;
        }
        if in_section && !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

fn read_dependency_names(manifest: &Path) -> BTreeSet<String> {
    read_section_lines(manifest, "dependencies")
        .into_iter()
        .filter_map(|line| {
            let (name, _) = line.split_once('=')?;
            Some(name.trim().to_string())
        })
        .collect()
}

fn dependency_line(manifest: &Path, section: &str, dep: &str) -> Option<String> {
    read_section_lines(manifest, section)
        .into_iter()
        .find(|line| {
            line.split_once('=')
                .is_some_and(|(name, _)| name.trim() == dep)
        })
}

fn extract_dependency_version(line: &str) -> Option<String> {
    let (_, rhs) = line.split_once('=')?;
    let rhs = rhs.trim();
    if let Some(stripped) = rhs.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let version_idx = rhs.find("version")?;
    let version_rhs = rhs[version_idx + "version".len()..]
        .trim_start_matches([' ', '='])
        .trim();
    let stripped = version_rhs.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(stripped[..end].to_string())
}

fn workspace_dependency_version(repo_root: &Path, dep: &str) -> Option<String> {
    let manifest = repo_root.join("Cargo.toml");
    dependency_line(&manifest, "workspace.dependencies", dep)
        .and_then(|line| extract_dependency_version(&line))
}

fn crate_dependency_version(repo_root: &Path, crate_manifest: &Path, dep: &str) -> Option<String> {
    let line = dependency_line(crate_manifest, "dependencies", dep)?;
    if line.contains("workspace") && line.contains("true") {
        workspace_dependency_version(repo_root, dep)
    } else {
        extract_dependency_version(&line)
    }
}

fn semver_compatibility_family(version: &str) -> Option<String> {
    let first = version.split(',').next()?.trim();
    let first = first.trim_start_matches(['^', '~', '=', '>', '<', ' ']);
    let mut parts = first.split('.');
    let major = parts.next()?.trim();
    if major.is_empty() {
        return None;
    }
    if major == "0" {
        let minor = parts.next()?.trim();
        if minor.is_empty() {
            return None;
        }
        Some(format!("{major}.{minor}"))
    } else {
        Some(major.to_string())
    }
}

timed_test!(
    cargo_toml_cargo_lock_and_package_json_share_one_version,
    Duration::from_secs(15),
    {
        if common::should_skip_source_tree_test(
            "cargo_toml_cargo_lock_and_package_json_share_one_version",
        ) {
            return;
        }

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

timed_test!(
    soldr_cli_and_embedded_zccache_shared_direct_deps_are_compatible,
    Duration::from_secs(15),
    {
        if common::should_skip_source_tree_test(
            "soldr_cli_and_embedded_zccache_shared_direct_deps_are_compatible",
        ) {
            return;
        }

        let root = repo_root();
        let soldr_cli_manifest = root.join("crates/soldr-cli/Cargo.toml");
        let zccache_root = root.join("_vender/zccache");
        let zccache_manifest = zccache_root.join("crates/zccache/Cargo.toml");

        let soldr_deps = read_dependency_names(&soldr_cli_manifest);
        let zccache_deps = read_dependency_names(&zccache_manifest);
        let intentionally_split = ["redb"];
        let mut checked = Vec::new();

        for dep in soldr_deps.intersection(&zccache_deps) {
            if intentionally_split.contains(&dep.as_str()) {
                continue;
            }
            let soldr_version = crate_dependency_version(&root, &soldr_cli_manifest, dep)
                .unwrap_or_else(|| panic!("soldr-cli dependency {dep} must carry a version"));
            let zccache_version = crate_dependency_version(&zccache_root, &zccache_manifest, dep)
                .unwrap_or_else(|| {
                    panic!("embedded zccache dependency {dep} must carry a version")
                });
            let soldr_family = semver_compatibility_family(&soldr_version).unwrap_or_else(|| {
                panic!("soldr-cli dependency {dep} has unsupported version {soldr_version:?}")
            });
            let zccache_family = semver_compatibility_family(&zccache_version).unwrap_or_else(|| {
                panic!("embedded zccache dependency {dep} has unsupported version {zccache_version:?}")
            });
            assert_eq!(
                soldr_family, zccache_family,
                "soldr#1356 dependency drift: shared direct dependency {dep} must stay \
                 semver-compatible between soldr-cli ({soldr_version}) and embedded zccache \
                 ({zccache_version}). If a split is intentional, document it in \
                 intentionally_split in this test."
            );
            checked.push(dep.clone());
        }

        assert!(
            checked.len() >= 10,
            "dependency drift guard checked too few shared deps: {checked:?}"
        );
    }
);
