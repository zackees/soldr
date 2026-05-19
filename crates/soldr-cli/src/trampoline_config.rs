//! Cargo config-file content hashing for the `soldr cargo run` trampoline
//! fingerprint (issue #346, slice 2 of #342).
//!
//! Cargo loads rustflags (and other build-affecting settings) from
//! `.cargo/config.toml` files walked from the manifest dir up to the
//! filesystem root, plus `$CARGO_HOME/config.toml`. The trampoline's
//! original fingerprint only hashed the `RUSTFLAGS` env var, so editing
//! `.cargo/config.toml` could leave the fast path serving a stale binary
//! whose rustflags no longer match the user's intent.
//!
//! We take the cheap fix: collect every relevant config-file path,
//! sort by canonical path, concatenate `path:\n<bytes>\n` per file, and
//! blake3-hash the result. That is over-aggressive — editing an unrelated
//! `[net]` section also busts the fingerprint and forces a rebuild on the
//! next `soldr cargo run` — but it is mechanically correct and free of
//! cargo subprocess calls. See issue #346 for the rationale.
//!
//! Lives in a sibling file referenced from `trampoline.rs` via `#[path]`
//! so `trampoline.rs` stays under the 1000-LOC ceiling (post-#339
//! convention).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Walk `manifest_dir` upward to the filesystem root, then `$CARGO_HOME`,
/// and produce a blake3 hash over the canonical bytes of every
/// `.cargo/config.toml` (and legacy `.cargo/config`) file found. Returns
/// `"blake3:<hex>"`.
///
/// I/O errors on any individual file are swallowed — that file is treated
/// as absent for hashing purposes. The trampoline must not crash on a
/// transient read failure; it should simply produce a stable-but-possibly-
/// stale hash and rely on the rest of the fingerprint to catch problems.
///
/// If no config files exist anywhere, the returned hash is the blake3 of
/// the empty input — a stable sentinel that lets the fingerprint round-
/// trip identically across runs in that state.
pub(crate) fn compute_cargo_config_hash(manifest_dir: &Path) -> String {
    let canonical_dir =
        fs::canonicalize(manifest_dir).unwrap_or_else(|_| manifest_dir.to_path_buf());
    let files = collect_config_files(&canonical_dir);
    let merged = merge_for_hash(&files);
    let hash = blake3::hash(&merged);
    format!("blake3:{}", hash.to_hex())
}

/// Public for tests: enumerate the canonical paths of every existing
/// cargo config file the hash will consume, sorted by canonical path
/// string. Files unreadable or missing are simply omitted.
pub(crate) fn collect_config_files(canonical_manifest_dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut by_path: BTreeMap<String, (PathBuf, Vec<u8>)> = BTreeMap::new();

    let mut walk_dir = Some(canonical_manifest_dir.to_path_buf());
    while let Some(dir) = walk_dir.take() {
        consider_cargo_dir(&dir.join(".cargo"), &mut by_path);
        let parent = dir.parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            if parent != dir {
                walk_dir = Some(parent);
            }
        }
    }

    if let Some(cargo_home) = cargo_home_dir() {
        consider_cargo_dir(&cargo_home, &mut by_path);
    }

    by_path.into_values().collect()
}

fn consider_cargo_dir(dir: &Path, sink: &mut BTreeMap<String, (PathBuf, Vec<u8>)>) {
    // Cargo prefers `config.toml`; the legacy `config` (no extension) is
    // still supported. We hash both if both exist — cargo warns in that
    // case but we want any visible config bytes in the fingerprint.
    for name in ["config.toml", "config"] {
        let candidate = dir.join(name);
        let bytes = match fs::read(&candidate) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let canonical = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        let key = canonical.to_string_lossy().to_string();
        sink.entry(key).or_insert((canonical, bytes));
    }
}

fn cargo_home_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("CARGO_HOME") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    home_dir().map(|h| h.join(".cargo"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            if !profile.is_empty() {
                return Some(PathBuf::from(profile));
            }
        }
        let drive = std::env::var_os("HOMEDRIVE");
        let path = std::env::var_os("HOMEPATH");
        if let (Some(d), Some(p)) = (drive, path) {
            if !d.is_empty() && !p.is_empty() {
                let mut joined = d;
                joined.push(p);
                return Some(PathBuf::from(joined));
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    }
}

fn merge_for_hash(files: &[(PathBuf, Vec<u8>)]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for (path, bytes) in files {
        let header = format!("{}:\n", path.to_string_lossy());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(bytes);
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    //! These tests assert deltas, not absolute hash values, so they don't
    //! need to isolate `$CARGO_HOME` / `$HOME` from the developer's real
    //! environment: whatever ancestor configs (and `~/.cargo/config*`)
    //! happen to exist contribute the same bytes on both halves of each
    //! pair, so the diff isolates the change we wrote to the tempdir.

    use super::*;
    use std::fs;

    fn tempdir(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("soldr-cfg-hash-{label}-"))
            .tempdir()
            .expect("tempdir")
    }

    #[test]
    fn empty_layout_returns_stable_hash() {
        let temp = tempdir("empty");
        let manifest_dir = temp.path().join("crate");
        fs::create_dir_all(&manifest_dir).expect("mk crate");

        let a = compute_cargo_config_hash(&manifest_dir);
        let b = compute_cargo_config_hash(&manifest_dir);
        assert_eq!(a, b, "missing-config hash must be deterministic");
        assert!(a.starts_with("blake3:"));
    }

    #[test]
    fn distinct_config_content_produces_distinct_hash() {
        let temp = tempdir("content");
        let crate_dir = temp.path().join("crate");
        let cargo_dir = crate_dir.join(".cargo");
        fs::create_dir_all(&cargo_dir).expect("mk .cargo");
        let cfg = cargo_dir.join("config.toml");

        fs::write(&cfg, "[build]\nrustflags = []\n").expect("write cfg");
        let h1 = compute_cargo_config_hash(&crate_dir);

        fs::write(&cfg, "[build]\nrustflags = [\"-C\", \"opt-level=0\"]\n").expect("rewrite cfg");
        let h2 = compute_cargo_config_hash(&crate_dir);

        assert_ne!(
            h1, h2,
            "editing .cargo/config.toml must change the config hash"
        );
    }

    #[test]
    fn identical_config_content_produces_identical_hash() {
        let temp = tempdir("identity");
        let crate_dir = temp.path().join("crate");
        let cargo_dir = crate_dir.join(".cargo");
        fs::create_dir_all(&cargo_dir).expect("mk .cargo");
        let cfg = cargo_dir.join("config.toml");
        fs::write(&cfg, "[build]\nrustflags = [\"-C\", \"debuginfo=0\"]\n").expect("write cfg");

        let h1 = compute_cargo_config_hash(&crate_dir);
        let h2 = compute_cargo_config_hash(&crate_dir);
        assert_eq!(h1, h2, "identical-content reads must hash identically");
    }

    #[test]
    fn adding_a_config_file_changes_the_hash() {
        let temp = tempdir("appear");
        let crate_dir = temp.path().join("crate");
        fs::create_dir_all(&crate_dir).expect("mk crate");

        let before = compute_cargo_config_hash(&crate_dir);

        let cargo_dir = crate_dir.join(".cargo");
        fs::create_dir_all(&cargo_dir).expect("mk .cargo");
        fs::write(
            cargo_dir.join("config.toml"),
            "[build]\nrustflags = [\"-C\", \"opt-level=1\"]\n",
        )
        .expect("write cfg");

        let after = compute_cargo_config_hash(&crate_dir);
        assert_ne!(
            before, after,
            "appearance of a new .cargo/config.toml must change the hash"
        );
    }

    #[test]
    fn parent_directory_config_is_included() {
        let temp = tempdir("parent");
        let parent = temp.path().join("workspace");
        let child = parent.join("crate");
        fs::create_dir_all(&child).expect("mk crate");
        let parent_cargo = parent.join(".cargo");
        fs::create_dir_all(&parent_cargo).expect("mk parent .cargo");
        let parent_cfg = parent_cargo.join("config.toml");

        fs::write(&parent_cfg, "[net]\nretry = 2\n").expect("write parent cfg");
        let h1 = compute_cargo_config_hash(&child);

        fs::write(&parent_cfg, "[net]\nretry = 7\n").expect("rewrite parent cfg");
        let h2 = compute_cargo_config_hash(&child);

        assert_ne!(
            h1, h2,
            "editing an ancestor .cargo/config.toml must change the hash"
        );
    }
}
