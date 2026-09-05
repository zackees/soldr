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

use crate::common;

/// Locate the soldr repo root from the CARGO_MANIFEST_DIR convention.
/// `CARGO_MANIFEST_DIR` points at `<repo>/crates/soldr-cli`; we want
/// `<repo>/`.
fn repo_root() -> PathBuf {
    common::workspace_root()
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

#[test]
fn cargo_toml_cargo_lock_and_package_json_share_one_version() {
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

#[test]
fn externalized_dependencies_are_exact_and_consistent() {
    let root = repo_root();
    let lock = fs::read_to_string(root.join("Cargo.lock"))
        .expect("read Cargo.lock")
        .replace("\r\n", "\n");

    for (dependency, version, manifests) in [
        (
            "zccache",
            "1.13.22",
            &[
                "crates/soldr-cli/Cargo.toml",
                "crates/soldr-cache/Cargo.toml",
                "crates/soldr-daemon/Cargo.toml",
            ][..],
        ),
        (
            "running-process",
            "4.10.9",
            &[
                "crates/soldr-cli/Cargo.toml",
                "crates/soldr-daemon/Cargo.toml",
                "crates/soldr-platform/Cargo.toml",
            ][..],
        ),
        (
            // soldr#3038: the `#[global_allocator]` dependency. Pinned to
            // the newest release satisfying zccache's own (unenabled,
            // optional) `"0.9.3"` requirement, so the two never resolve to
            // two different copies in the same binary if a future zccache
            // feature turns its dependency on.
            "mimalloc-pprof",
            "0.9.5",
            &[
                "crates/soldr-cli/Cargo.toml",
                "crates/soldr-daemon/Cargo.toml",
            ][..],
        ),
    ] {
        let locked = format!("name = \"{dependency}\"\nversion = \"{version}\"");
        assert!(
            lock.contains(&locked),
            "Cargo.lock must resolve {dependency} {version}"
        );
        for relative in manifests {
            let manifest = root.join(relative);
            let line = dependency_line(&manifest, "dependencies", dependency)
                .unwrap_or_else(|| panic!("{relative} must depend on {dependency}"));
            let expected = format!("={version}");
            assert_eq!(
                extract_dependency_version(&line).as_deref(),
                Some(expected.as_str()),
                "{relative} must pin the exact released {dependency} version"
            );
            assert!(
                !line.contains("path") && !line.contains("git"),
                "{relative} must resolve {dependency} from the registry: {line}"
            );
        }
    }
}

#[test]
fn external_zccache_profiles_bound_internal_codegen_parallelism() {
    let root = repo_root();
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest");

    for profile in ["dev", "test", "ci-bootstrap", "ci-release", "ci-nextest"] {
        let section = format!("profile.{profile}.package.zccache");
        let lines = read_section_lines(&root.join("Cargo.toml"), &section);
        assert!(
            lines.iter().any(|line| line == "codegen-units = 1"),
            "[{section}] must keep the amalgamated zccache unit single-codegen"
        );
    }
    assert!(manifest.contains("[profile.dev.package.zccache]"));
}
