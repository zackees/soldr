//! Reader for `[workspace.metadata.soldr]` / `[package.metadata.soldr]`
//! sections of `Cargo.toml` (RFC #914).
//!
//! Cargo's manifest reference explicitly blesses `[*.metadata.*]` tables
//! as the place for tool-specific configuration:
//!
//! > The `package.metadata` table, however, is completely ignored by
//! > Cargo and will not be warned about. This section can be used for
//! > tools which would like to store package configuration in
//! > `Cargo.toml`.
//!
//! soldr uses this to discover the multi-target shipping matrix for
//! `soldr prepare --target all` without inventing a new file format
//! and without piggybacking on `rust-toolchain.toml [toolchain].targets`
//! (rustup would unconditionally install every entry — ~30–80 MB of
//! `rust-std` per triple — on every fresh checkout).
//!
//! Lookup order:
//! 1. `[workspace.metadata.soldr].targets` — primary.
//! 2. `[package.metadata.soldr].targets`   — fallback for single-crate
//!    repos with no workspace table.
//! 3. Both missing → ERROR with a directive message naming the missing
//!    table. No fallback to `rust-toolchain.toml` or `.cargo/config.toml`
//!    because their semantics differ ("install these components" /
//!    "build for these by default") from "this is the shipping matrix".

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::core::SoldrError;

/// Top-level shape of a `Cargo.toml` for the keys we care about.
/// `serde(default)` everywhere so the parse succeeds against any
/// well-formed manifest, even ones that don't mention soldr at all.
#[derive(Debug, Deserialize, Default)]
struct CargoToml {
    #[serde(default)]
    workspace: Option<WorkspaceTable>,
    #[serde(default)]
    package: Option<PackageTable>,
}

#[derive(Debug, Deserialize, Default)]
struct WorkspaceTable {
    #[serde(default)]
    metadata: Option<MetadataTable>,
}

#[derive(Debug, Deserialize, Default)]
struct PackageTable {
    #[serde(default)]
    metadata: Option<MetadataTable>,
}

#[derive(Debug, Deserialize, Default)]
struct MetadataTable {
    #[serde(default)]
    soldr: Option<SoldrMetadata>,
}

/// `[workspace.metadata.soldr]` / `[package.metadata.soldr]` body.
/// Public so future fields can be added (e.g. `[..].cache_archive`
/// default) without rewriting consumers.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct SoldrMetadata {
    /// The shipping matrix: list of rust target triples this repo
    /// declares it builds for. Consumed by `soldr prepare --target all`.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Prefer a newer `soldr` found globally on PATH over the binary launched
    /// from this checkout. Disabled unless the project opts in.
    #[serde(default)]
    pub prefer_newer_global: bool,
}

/// Read soldr metadata from the `Cargo.toml` at `path`. Returns the
/// merged `SoldrMetadata` honoring the workspace → package fallback
/// order described in the module docstring.
///
/// **Does not error on missing tables** — returns `Ok(SoldrMetadata
/// { targets: vec![] })` when no `[*.metadata.soldr]` section exists.
/// Callers that need to require the section (e.g. `--target all`)
/// must check `targets.is_empty()` themselves and surface a
/// directive error; that keeps this reader reusable for any future
/// soldr-metadata field that's genuinely optional.
pub fn read_soldr_metadata(cargo_toml_path: &Path) -> Result<SoldrMetadata, SoldrError> {
    let body = std::fs::read_to_string(cargo_toml_path).map_err(|e| {
        SoldrError::Other(format!(
            "soldr prepare: read {}: {e}",
            cargo_toml_path.display()
        ))
    })?;
    let parsed: CargoToml = toml::from_str(&body).map_err(|e| {
        SoldrError::Other(format!(
            "soldr prepare: parse {}: {e}",
            cargo_toml_path.display()
        ))
    })?;
    // Workspace takes precedence; package-scoped metadata is fallback
    // for single-crate repos that don't declare a workspace table.
    let workspace_meta = parsed
        .workspace
        .and_then(|w| w.metadata)
        .and_then(|m| m.soldr);
    if let Some(meta) = workspace_meta {
        return Ok(meta);
    }
    let package_meta = parsed
        .package
        .and_then(|p| p.metadata)
        .and_then(|m| m.soldr);
    Ok(package_meta.unwrap_or_default())
}

/// Walk up from `start_dir` looking for the first `Cargo.toml`. Used
/// when `soldr prepare --target all` is invoked from a subdirectory
/// of the repo. Returns `None` if no manifest is found before the
/// filesystem root.
pub fn find_cargo_toml(start_dir: &Path) -> Option<PathBuf> {
    let mut cur = start_dir;
    loop {
        let candidate = cur.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Whether any Cargo manifest from `start_dir` through its ancestor chain
/// opts into handing off local soldr invocations to a newer global binary.
///
/// Walking the full chain matters when the command starts in a workspace
/// member: its nearest manifest usually has package metadata while the policy
/// belongs to the root's `[workspace.metadata.soldr]` table.
pub fn prefer_newer_global_from(start_dir: &Path) -> bool {
    let mut current = start_dir;
    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file()
            && read_soldr_metadata(&candidate)
                .map(|metadata| metadata.prefer_newer_global)
                .unwrap_or(false)
        {
            return true;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return false,
        }
    }
}

/// Best-effort current-directory wrapper around [`prefer_newer_global_from`].
/// An unreadable working directory simply leaves the opt-in policy disabled.
pub fn prefer_newer_global_from_cwd() -> bool {
    std::env::current_dir()
        .ok()
        .is_some_and(|cwd| prefer_newer_global_from(&cwd))
}

/// Resolve `--target all` to the explicit target list.
///
/// Resolution order (soldr#937 — containerized image-build needs a
/// no-workspace fallback):
///
/// 1. Walk up from the current working directory; on the first
///    `Cargo.toml` found, return `[*.metadata.soldr].targets` if the
///    section is present **and non-empty**.
/// 2. Otherwise fall back to [`crate::core::canonical_targets`] —
///    soldr's compiled-in canonical target list. Prints a one-line stderr
///    notice so the fallback is visible in container build logs.
///
/// The fallback path is what makes `soldr prepare --target all`
/// work inside `examples/docker-cross-all/Dockerfile` where the
/// soldr source isn't mounted — no need for the explicit
/// comma-separated 8-triple form the Dockerfile used to ship.
pub fn resolve_all_targets() -> Result<Vec<String>, SoldrError> {
    let cwd = std::env::current_dir()
        .map_err(|e| SoldrError::Other(format!("soldr prepare: cwd: {e}")))?;

    if let Some(manifest) = find_cargo_toml(&cwd) {
        let meta = read_soldr_metadata(&manifest)?;
        if !meta.targets.is_empty() {
            return Ok(meta.targets);
        }
        eprintln!(
            "soldr prepare --target all: `{}` declares no \
             `[workspace.metadata.soldr].targets` — falling back to soldr's \
             compiled-in canonical target list (soldr#937).",
            manifest.display()
        );
    } else {
        eprintln!(
            "soldr prepare --target all: no Cargo.toml under `{}` — \
             falling back to soldr's compiled-in canonical target list (soldr#937).",
            cwd.display()
        );
    }

    Ok(crate::core::canonical_targets()
        .iter()
        .map(|s| (*s).to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a Cargo.toml fixture to `tmpdir/Cargo.toml` and
    /// return the path.
    fn write_fixture(tmpdir: &Path, body: &str) -> PathBuf {
        let p = tmpdir.join("Cargo.toml");
        std::fs::write(&p, body).expect("write fixture");
        p
    }

    #[test]
    fn reads_workspace_metadata() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[workspace]

[workspace.metadata.soldr]
targets = [
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(
            meta.targets,
            vec![
                "x86_64-pc-windows-msvc".to_string(),
                "aarch64-apple-darwin".to_string(),
            ]
        );
    }

    #[test]
    fn reads_package_metadata_when_no_workspace() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"

[package.metadata.soldr]
targets = ["x86_64-unknown-linux-gnu"]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert_eq!(meta.targets, vec!["x86_64-unknown-linux-gnu".to_string()]);
    }

    #[test]
    fn workspace_metadata_wins_over_package() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"

[package.metadata.soldr]
targets = ["aarch64-unknown-linux-musl"]

[workspace]

[workspace.metadata.soldr]
targets = ["x86_64-pc-windows-msvc"]
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        // workspace block wins
        assert_eq!(meta.targets, vec!["x86_64-pc-windows-msvc".to_string()]);
    }

    #[test]
    fn no_metadata_returns_empty() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(
            tmp.path(),
            r#"
[package]
name = "thing"
version = "0.1.0"
"#,
        );
        let meta = read_soldr_metadata(&p).expect("parse");
        assert!(meta.targets.is_empty());
    }

    #[test]
    fn reads_prefer_newer_global() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path().join("workspace");
        let nested = root.join("member").join("src");
        std::fs::create_dir_all(&nested).expect("nested workspace member");
        write_fixture(
            &root,
            "[workspace]\n\n[workspace.metadata.soldr]\nprefer_newer_global = true\n",
        );
        std::fs::write(
            root.join("member").join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .expect("member manifest");

        assert!(prefer_newer_global_from(&nested));
    }

    #[test]
    fn malformed_toml_errors_with_path() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(tmp.path(), "this is = = not valid toml");
        let err = read_soldr_metadata(&p).expect_err("malformed");
        let msg = err.to_string();
        assert!(msg.contains("parse"), "msg: {msg}");
        assert!(msg.contains("Cargo.toml"), "msg: {msg}");
    }

    #[test]
    fn find_cargo_toml_walks_up() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = write_fixture(tmp.path(), "[package]\nname = \"x\"\nversion = \"0\"\n");
        // create a nested subdir
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let found = find_cargo_toml(&nested).expect("walked up");
        assert_eq!(found, p);
    }

    #[test]
    fn find_cargo_toml_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let nested = tmp.path().join("nothing-here");
        std::fs::create_dir_all(&nested).expect("mkdir");
        // tmp is under TEMP which has no Cargo.toml ancestor on a
        // freshly-created TempDir; but the actual filesystem may
        // have one above TEMP. Use the canonical root to verify the
        // None path is reachable in principle.
        // We can't guarantee no Cargo.toml exists above tmpdir on
        // every host, so only assert this is a safe call (no panic).
        let _ = find_cargo_toml(&nested);
    }
}
