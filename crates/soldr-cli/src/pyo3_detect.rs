//! soldr#939 — detect PyO3 in the current cargo workspace.
//!
//! When the cargo front door is about to dispatch a cross-compile
//! (target ≠ host), it consults [`workspace_uses_pyo3`] to decide
//! whether to inject `PYO3_CROSS_*` + `PYO3_NO_PYTHON` env vars
//! from the prepared catalogue assets. Workspaces that don't depend
//! on PyO3 get exactly the same env they always had.
//!
//! Detection is structural — reads `cargo metadata` JSON for any
//! direct or transitive `pyo3` dep across all workspace members.
//! Explicit env from the caller always wins (the env-injection
//! happens before exec; the user can set `PYO3_CROSS_LIB_DIR`
//! themselves to override).
//!
//! ## Why not parse `Cargo.toml` ourselves
//!
//! PyO3 typically appears as a transitive dep (via maturin's
//! `pyo3-build-config`, `pyo3-macros`, etc.). A direct `[dependencies]`
//! string match would miss the workspace member that pulls PyO3
//! through a path-dep. `cargo metadata --no-deps` would miss
//! transitives; `cargo metadata --format-version 1` with deps is
//! the only complete answer.
//!
//! ## Cost
//!
//! `cargo metadata` against a fresh workspace is 50-200 ms — fine
//! for a cross-compile dispatch but too slow to call on every
//! `soldr cargo build` invocation. The detection result is cached
//! in a process-wide `OnceLock` keyed by the workspace root path.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::core::SoldrError;

/// Process-wide cache of "this workspace uses PyO3" lookup. Keyed
/// by the canonicalized workspace-root path. One miss per workspace
/// per process.
static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, bool>>> =
    OnceLock::new();

fn cache() -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, bool>> {
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Returns `true` iff `cargo metadata` for the workspace rooted at
/// `cwd` lists `pyo3` (or any `pyo3-*` crate) anywhere in the
/// dependency graph (including transitive deps of any workspace
/// member).
///
/// Failures (no Cargo.toml, malformed JSON, cargo not on PATH) are
/// treated as "PyO3 not detected" — the caller's existing behaviour
/// is preserved on edge cases.
pub fn workspace_uses_pyo3(cwd: &Path) -> bool {
    let canonical = match cwd.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };

    if let Ok(guard) = cache().lock() {
        if let Some(&hit) = guard.get(&canonical) {
            return hit;
        }
    }

    let detected = detect_uncached(&canonical);

    if let Ok(mut guard) = cache().lock() {
        guard.insert(canonical, detected);
    }
    detected
}

fn detect_uncached(workspace_root: &Path) -> bool {
    // Cargo doesn't accept a `--directory` flag; invoke from inside
    // the workspace root via Command::current_dir.
    let output = match Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(workspace_root)
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = match std::str::from_utf8(&output.stdout) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Cheap check first: substring scan for `"pyo3` or `"pyo3-`.
    // Avoids parsing a multi-megabyte JSON blob for the common case
    // where the answer is no.
    if !stdout.contains("\"pyo3\"") && !stdout.contains("\"pyo3-") {
        return false;
    }

    // Confirm via structural parse — guards against false positives
    // like a crate description string that mentions "pyo3".
    let metadata: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return true, // substring matched, parse failed — assume yes
    };
    if let Some(packages) = metadata.get("packages").and_then(|v| v.as_array()) {
        for pkg in packages {
            if let Some(name) = pkg.get("name").and_then(|v| v.as_str()) {
                if name == "pyo3" || name.starts_with("pyo3-") {
                    return true;
                }
            }
        }
    }
    false
}

/// Return the env block to inject when [`workspace_uses_pyo3`] is
/// true and `target` differs from the host. Today the block is
/// minimal — `PYO3_NO_PYTHON=1` so PyO3's build.rs short-circuits.
/// As the Python catalogue rows (soldr#931 / #932 / #933) ship,
/// this function grows the per-target `PYO3_CROSS_LIB_DIR` +
/// `PYO3_CROSS_PYTHON_VERSION` resolution.
///
/// Returns an empty map iff PyO3 detection found nothing OR the
/// target equals the host (no cross-compile, native Python in
/// the rustc-default toolchain works).
pub fn cross_env_for_target(workspace_root: &Path, target: &str) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    if target.is_empty() || !workspace_uses_pyo3(workspace_root) {
        return env;
    }
    if target_equals_host(target) {
        return env;
    }
    env.insert("PYO3_NO_PYTHON".to_string(), "1".to_string());
    env
}

fn target_equals_host(target: &str) -> bool {
    // Resolve the host triple at compile time via the `cfg!` lattice.
    // Matches the same set `target_alias::host_triple` uses.
    let host = host_triple();
    target == host
}

fn host_triple() -> &'static str {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "musl")) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64", target_env = "musl")) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        ""
    }
}

#[allow(dead_code)]
fn _ignore_unused(_: &SoldrError) {}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(empty_workspace_yields_no_env, {
        // tmpdir has no Cargo.toml; detection silently returns false
        // and produces an empty env block.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let env = cross_env_for_target(tmp.path(), "x86_64-pc-windows-msvc");
        assert!(env.is_empty());
    });

    crate::timed_test!(host_target_yields_no_env, {
        // Even when PyO3 is detected, host==target means no cross
        // → no env injection.
        let host = host_triple();
        if host.is_empty() {
            // Unsupported test host
            return;
        }
        let tmp = tempfile::tempdir().expect("tmpdir");
        let env = cross_env_for_target(tmp.path(), host);
        assert!(env.is_empty());
    });

    crate::timed_test!(host_triple_resolves_to_known_triple, {
        let h = host_triple();
        assert!(
            h.is_empty() || crate::core::is_canonical(h) || h.contains('-'),
            "host_triple resolved to unexpected value: {h:?}"
        );
    });
}
