//! soldr#1012 PR 5 — blessed cross-compile sysroot preparation for
//! `soldr build --target <T>`.
//!
//! When `Commands::Build` (soldr#1013) detects a canonical cross-
//! compile target, this module materializes the per-target sysroot
//! from the soldr-toolchain catalogue, sets the env vars cargo + cc-rs
//! + linker tools need, and installs the clang shim ahead of the
//! system clang on PATH.
//!
//! ## Target coverage
//!
//! * `*-pc-windows-msvc` — xwin-cache + clang shim (sidesteps the
//!   ring 0.17.14:563 oversight described in [`crate::fetch::xwin_cache`]
//!   and the `soldr-clang-shim` binary doc)
//! * `*-apple-darwin` — soldr's existing apple-sdk fetcher already
//!   handles this via `soldr prepare`; the blessed `soldr build` path
//!   just calls into [`crate::fetch::apple_sdk::ensure_apple_sdk`]
//!
//! Other targets (linux musl, linux gnu) get no sysroot prep — they
//! work out-of-the-box with the host cargo + zigbuild flow.
//!
//! ## Opt-out
//!
//! `SOLDR_USE_LEGACY_XWIN=1` (or `SOLDR_USE_LEGACY_ZIGBUILD=1`)
//! suppresses the blessed prep for that toolchain family and falls
//! through to the cargo-xwin / cargo-zigbuild path. Mirrors the
//! `SOLDR_USE_LEGACY_*` env-var contract documented in soldr#1010
//! Phase 8.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

/// Env var that opts out of the blessed xwin-cache materialization
/// and falls through to cargo-xwin's own live download for win-msvc
/// targets. Set to any non-empty value to trigger.
pub const USE_LEGACY_XWIN_ENV_VAR: &str = "SOLDR_USE_LEGACY_XWIN";

/// Env var that opts out of the blessed zigbuild prep flow. Reserved
/// for future #1012 work; today's zigbuild path is unchanged.
pub const USE_LEGACY_ZIGBUILD_ENV_VAR: &str = "SOLDR_USE_LEGACY_ZIGBUILD";

/// What the blessed-build prep accomplished, returned to the caller
/// so `Commands::Build` can log + set the resulting env vars on the
/// child cargo invocation.
#[derive(Debug, Clone, Default)]
pub struct BlessedPrep {
    /// Path to `XWIN_CACHE_DIR` value (for `*-pc-windows-msvc`).
    pub xwin_cache_dir: Option<PathBuf>,
    /// Path to `SDKROOT` value (for `*-apple-darwin`).
    pub sdkroot: Option<PathBuf>,
    /// Directory to prepend to `PATH` so the soldr-clang-shim wins
    /// when cc-rs (or ring) does a bare `which("clang")` lookup.
    pub shim_path_dir: Option<PathBuf>,
    /// Per-target env-var tuples to set on the child cargo
    /// invocation: `[(NAME, VALUE), ...]`. Includes `CC_<t>`,
    /// `CXX_<t>`, `AR_<t>`, `CARGO_TARGET_<T>_LINKER` for MSVC
    /// targets.
    pub env: Vec<(String, String)>,
}

/// Prepare the blessed sysroot for `target`. Returns `Ok(prep)` on
/// success; `Ok(BlessedPrep::default())` if `target` doesn't need
/// any prep (or if the caller opted into legacy via env vars);
/// `Err(_)` only on actual fetch / extract failures.
///
/// Caller is responsible for applying `prep.env` and prepending
/// `prep.shim_path_dir` to `PATH` on the child cargo invocation.
pub async fn prepare(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<BlessedPrep, SoldrError> {
    let mut prep = BlessedPrep::default();

    // ----------------------------- Windows MSVC ------------------------------
    if target_triple.ends_with("-pc-windows-msvc") && !legacy_xwin_opt_out() {
        // Install the clang shim + set cc-rs env vars FIRST,
        // independent of xwin-cache state. The shim is what actually
        // fixes ring's hardcoded compiler override (build.rs:563);
        // xwin-cache is only an optimization that lets cargo-xwin
        // short-circuit its live MSVC download. Even when the
        // catalogue row for a target arch isn't yet ingested (arm64
        // today, until soldr-toolchain PR #30's recipe gets dispatched
        // + ingested), the shim alone makes ring compile correctly —
        // cargo-xwin's live download still works for the SDK itself.
        let target_u = target_triple.replace('-', "_");
        let target_u_upper = target_u.to_uppercase();

        let shim_dir = install_clang_shim(paths)?;
        prep.shim_path_dir = Some(shim_dir);

        prep.env.push((format!("CC_{target_u}"), "clang".to_string()));
        prep.env
            .push((format!("CXX_{target_u}"), "clang".to_string()));
        prep.env
            .push((format!("AR_{target_u}"), "llvm-lib".to_string()));
        prep.env.push((
            format!("CARGO_TARGET_{target_u_upper}_LINKER"),
            "lld-link".to_string(),
        ));

        // Now try to materialize xwin-cache too. If the catalogue row
        // for this arch is ingested, we set XWIN_CACHE_DIR so cargo-
        // xwin transparently uses our cache instead of triggering a
        // fresh live download. If not, fall through — the shim alone
        // is enough for the win-arm64 ring fix; cargo-xwin's live
        // download still produces a working SDK.
        match crate::fetch::xwin_cache::ensure_xwin_cache(paths, target_triple).await {
            Ok(cache_dir) => {
                prep.xwin_cache_dir = Some(cache_dir.clone());
                prep.env.push((
                    crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR.to_string(),
                    cache_dir.to_string_lossy().into_owned(),
                ));
            }
            Err(e) => {
                eprintln!(
                    "soldr build: catalogue xwin-cache unavailable for {target_triple}: {e}"
                );
                eprintln!(
                    "soldr build: continuing without XWIN_CACHE_DIR — \
                     cargo-xwin's live download will produce the SDK \
                     (clang shim is still active for ring's compile fix)"
                );
            }
        }
    }

    // ------------------------------ Apple Darwin -----------------------------
    if target_triple.ends_with("-apple-darwin") && !legacy_zigbuild_opt_out() {
        // Apple SDK fetch is the same code path `soldr prepare` uses,
        // so this is reuse rather than new logic.
        match crate::fetch::apple_sdk::ensure_apple_sdk(paths).await {
            Ok(sdk) => {
                prep.sdkroot = Some(sdk.clone());
                prep.env.push((
                    "SDKROOT".to_string(),
                    sdk.to_string_lossy().into_owned(),
                ));
            }
            Err(e) => {
                eprintln!(
                    "soldr build: apple SDK unavailable for {target_triple}: {e}"
                );
                // Don't hard-fail; zigbuild may still locate an SDK
                // via cargo-zigbuild's own mechanism.
            }
        }
    }

    Ok(prep)
}

fn legacy_xwin_opt_out() -> bool {
    std::env::var(USE_LEGACY_XWIN_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn legacy_zigbuild_opt_out() -> bool {
    std::env::var(USE_LEGACY_ZIGBUILD_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Install the soldr-clang-shim binary at `<paths.bin>/clang(.exe)`
/// and matching siblings (`clang++`). Idempotent — overwrites if
/// already present (cheap; the shim is ~few hundred KB).
///
/// Returns the directory the shim was installed into, ready for
/// PATH prepending.
fn install_clang_shim(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;

    let shim_dir = paths.bin.join("clang-shim");
    std::fs::create_dir_all(&shim_dir)?;

    // The `soldr-clang-shim` binary lives next to the soldr binary
    // (both are produced by the same workspace build). On a soldr-
    // released install, it's at `<exe-dir>/soldr-clang-shim(.exe)`.
    let shim_src = locate_shim_binary()?;

    for name in clang_shim_names() {
        let dst = shim_dir.join(&name);
        // Best-effort remove first; ignore errors (file may not exist).
        let _ = std::fs::remove_file(&dst);
        // Use std::fs::copy for cross-platform; symlinks would be
        // cleaner on POSIX but the implementation cost isn't worth
        // it for what's effectively a 0-cost-per-invocation file copy.
        std::fs::copy(&shim_src, &dst).map_err(|e| {
            SoldrError::Other(format!(
                "failed to install soldr-clang-shim at {}: {e}",
                dst.display()
            ))
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dst)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dst, perms)?;
        }
    }

    Ok(shim_dir)
}

#[cfg(windows)]
fn clang_shim_names() -> Vec<String> {
    // Only `clang` + `clang++` — soldr-clang-shim's `from_argv0` accepts
    // those two basenames (plus `soldr-clang-shim` itself). DO NOT add
    // `clang-cl` here: the shim invokes `clang-cl` as its DOWNSTREAM (it
    // exists to route clang→clang-cl), and if `clang-cl` is also a
    // symlink to the shim then PATH resolution finds the shim's own
    // clang-cl symlink first and the shim re-invokes itself with
    // argv[0]=clang-cl → `unrecognized argv[0] basename`. See
    // ci/docker-aarch64-windows-msvc-cross/ + soldr#1033 followup.
    vec![
        "clang.exe".to_string(),
        "clang++.exe".to_string(),
    ]
}

#[cfg(not(windows))]
fn clang_shim_names() -> Vec<String> {
    // See the Windows branch for why clang-cl is NOT here.
    vec![
        "clang".to_string(),
        "clang++".to_string(),
    ]
}

fn locate_shim_binary() -> Result<PathBuf, SoldrError> {
    // Look next to the running `soldr` executable.
    let current_exe = std::env::current_exe().map_err(|e| {
        SoldrError::Other(format!("could not resolve current exe: {e}"))
    })?;
    let exe_dir = current_exe.parent().ok_or_else(|| {
        SoldrError::Other("current exe has no parent directory".to_string())
    })?;

    let shim_name = if cfg!(windows) {
        "soldr-clang-shim.exe"
    } else {
        "soldr-clang-shim"
    };
    let candidate = exe_dir.join(shim_name);
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(SoldrError::Other(format!(
        "soldr-clang-shim binary not found at {}; expected next to the \
         running soldr exe. Reinstall soldr or rebuild from source.",
        candidate.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(opt_out_env_var_recognized, {
        let prev = std::env::var_os(USE_LEGACY_XWIN_ENV_VAR);

        std::env::remove_var(USE_LEGACY_XWIN_ENV_VAR);
        assert!(!legacy_xwin_opt_out());

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "1");
        assert!(legacy_xwin_opt_out());

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "0");
        assert!(!legacy_xwin_opt_out(), "literal '0' must not opt in");

        std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, "");
        assert!(!legacy_xwin_opt_out(), "empty value must not opt in");

        match prev {
            Some(v) => std::env::set_var(USE_LEGACY_XWIN_ENV_VAR, v),
            None => std::env::remove_var(USE_LEGACY_XWIN_ENV_VAR),
        }
    });

    crate::timed_test!(linux_targets_get_no_prep, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let prep = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(prepare(&paths, "x86_64-unknown-linux-musl"))
            .expect("linux musl target should not error");
        assert!(prep.xwin_cache_dir.is_none());
        assert!(prep.sdkroot.is_none());
        assert!(prep.env.is_empty());
    });
}
