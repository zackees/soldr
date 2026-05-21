//! `.cargo/config.toml` walker + content digest for the trampoline
//! fingerprint (issue #346).
//!
//! The trampoline previously hashed only `RUSTFLAGS` from the environment.
//! Cargo also reads rustflags (and a pile of other build-affecting
//! settings) from `.cargo/config.toml`. Editing the config file silently
//! left the fingerprint untouched, so the trampoline kept fast-pathing
//! the stale binary.
//!
//! Mirroring cargo's full config resolution (precedence, target tables,
//! `--config` CLI overrides, etc.) is a non-trivial amount of code. The
//! issue (#346) explicitly accepts a simpler tradeoff for slice 2 of
//! #342: hash the raw bytes of every `.cargo/config.toml` cargo could
//! see, mix the digest into the fingerprint, and accept that editing an
//! unrelated section (`[net]`, `[registries]`, etc.) will also bust the
//! fingerprint.
//!
//! Walk order matches cargo's documented hierarchical lookup:
//!   1. From the manifest dir upward to filesystem root, checking
//!      `.cargo/config.toml` and the legacy `.cargo/config` at each
//!      level.
//!   2. `$CARGO_HOME/config.toml` (and legacy `$CARGO_HOME/config`),
//!      where `CARGO_HOME` defaults to `~/.cargo`.
//!
//! Files that don't exist contribute nothing — appearing or disappearing
//! changes the digest exactly as if their bytes changed.
//!
//! The digest is intentionally cheap: blake3 over a sorted list of
//! `(canonical-path, bytes)` pairs. No TOML parsing, no canonical
//! re-serialization. We pay the cost of reading a handful of small
//! config files on every trampoline check — small change compared to
//! either spawning cargo or recompiling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Compute the cargo-config digest mixed into the trampoline
/// fingerprint. Returns `"blake3:<hex>"`. The digest covers every
/// `.cargo/config.toml` (+ legacy `.cargo/config`) discovered by walking
/// from `manifest_dir` up to the filesystem root, plus
/// `$CARGO_HOME/config.toml` (default `~/.cargo/config.toml`).
///
/// Missing files contribute nothing; appearing or disappearing changes
/// the digest exactly as if file content had been edited.
pub(crate) fn cargo_config_digest(manifest_dir: &Path) -> String {
    let files = discover_cargo_config_files(manifest_dir);
    let mut hasher = blake3::Hasher::new();
    // Domain-separate this digest so a future caller can never accidentally
    // collide with another blake3 input mixed into the same fingerprint.
    hasher.update(b"soldr-trampoline-cargo-config-v1\0");
    for path in &files {
        let path_str = path.to_string_lossy();
        // Length-prefix the path so the boundary between path and bytes
        // is unambiguous (otherwise "ab" + "cd" hashes the same as "abc"
        // + "d").
        let path_bytes = path_str.as_bytes();
        hasher.update(&(path_bytes.len() as u64).to_le_bytes());
        hasher.update(path_bytes);
        match std::fs::read(path) {
            Ok(bytes) => {
                hasher.update(&(bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
            }
            Err(_) => {
                // Race: file existed during discovery, gone now. Treat
                // as zero-length; the next invocation will pick up the
                // real state.
                hasher.update(&0u64.to_le_bytes());
            }
        }
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Walk from `manifest_dir` upward and collect every cargo config file
/// cargo would consider, plus the per-user config under `$CARGO_HOME`.
///
/// Returned paths are canonicalized when possible and deduplicated. The
/// order is ascending by canonical path string so the resulting digest
/// is stable regardless of discovery order.
pub(crate) fn discover_cargo_config_files(manifest_dir: &Path) -> Vec<PathBuf> {
    // BTreeMap keeps the final list sorted by canonical path string and
    // dedupes naturally — important on Windows where `\\?\` prefixes and
    // case folding can produce duplicates.
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();

    // 1. Walk the manifest dir up to the filesystem root.
    let mut cursor: Option<PathBuf> = Some(manifest_dir.to_path_buf());
    while let Some(dir) = cursor {
        for candidate in cargo_config_candidates_in(&dir) {
            insert_if_present(&mut seen, &candidate);
        }
        cursor = dir.parent().map(Path::to_path_buf);
    }

    // 2. $CARGO_HOME/config.toml and the legacy $CARGO_HOME/config.
    if let Some(home) = cargo_home_dir() {
        for candidate in cargo_config_candidates_in(&home) {
            insert_if_present(&mut seen, &candidate);
        }
    }

    seen.into_values().collect()
}

/// Paths cargo will look for inside a given directory's `.cargo/`
/// subdirectory. `config.toml` is the modern name; bare `config` is the
/// legacy fallback cargo still honors. Order doesn't matter — the caller
/// sorts.
fn cargo_config_candidates_in(dir: &Path) -> [PathBuf; 2] {
    let cargo_dir = dir.join(".cargo");
    [cargo_dir.join("config.toml"), cargo_dir.join("config")]
}

/// $CARGO_HOME → directory holding cargo's per-user config. Default is
/// `~/.cargo`. Returns `None` if neither env var nor home dir is
/// resolvable, which means we just skip the per-user config (the walk
/// of the manifest dir still contributes).
fn cargo_home_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO_HOME") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    home_dir().map(|h| h.join(".cargo"))
}

/// Cross-platform `~`. We don't pull in `dirs` for this — only one
/// caller, and `$HOME` / `$USERPROFILE` cover the practical cases.
fn home_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("HOME") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    #[cfg(windows)]
    {
        if let Some(v) = std::env::var_os("USERPROFILE") {
            if !v.is_empty() {
                return Some(PathBuf::from(v));
            }
        }
    }
    None
}

/// Insert `candidate` into `seen` keyed by canonical path string if the
/// file exists. Canonicalization is best-effort — if it fails we fall
/// back to the raw path so we still record the file content.
fn insert_if_present(seen: &mut BTreeMap<String, PathBuf>, candidate: &Path) {
    if !candidate.is_file() {
        return;
    }
    let canonical = std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    let key = canonical.to_string_lossy().to_string();
    seen.entry(key).or_insert(canonical);
}

#[cfg(test)]
#[path = "trampoline_config_tests.rs"]
mod tests;
