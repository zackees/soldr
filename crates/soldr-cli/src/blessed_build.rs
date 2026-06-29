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
        // fresh live download, AND we inject the `/imsvc` include flags
        // + `/LIBPATH:` linker args via `CFLAGS_<t>` / `CXXFLAGS_<t>` /
        // `CARGO_TARGET_<T>_RUSTFLAGS` so the blessed path works
        // natively without cargo-xwin in the loop (soldr#1036 — make
        // soldr build a true cargo-xwin replacement, not just a wrapper
        // that delegates to it). If the catalogue row isn't ingested,
        // fall through — the shim alone is enough for the win-arm64
        // ring fix; cargo-xwin's live download still produces a working
        // SDK.
        match crate::fetch::xwin_cache::ensure_xwin_cache(paths, target_triple).await {
            Ok(cache_dir) => {
                prep.xwin_cache_dir = Some(cache_dir.clone());
                prep.env.push((
                    crate::fetch::xwin_cache::XWIN_CACHE_DIR_ENV_VAR.to_string(),
                    cache_dir.to_string_lossy().into_owned(),
                ));

                // Native cflags + rustflags injection (soldr#1036).
                // cc-rs reads `CFLAGS_<target>` / `CXXFLAGS_<target>`
                // and forwards them to the compiler invocation. The
                // soldr-clang-shim routes `clang` → `clang-cl` for
                // *-pc-windows-msvc, and clang-cl natively accepts
                // `/imsvc <path>` MSVC-style include flags.
                let cflags = xwin_msvc_cflags(&cache_dir);
                if !cflags.is_empty() {
                    prep.env.push((format!("CFLAGS_{target_u}"), cflags.clone()));
                    prep.env.push((format!("CXXFLAGS_{target_u}"), cflags));
                }

                // Linker args via cargo's per-target RUSTFLAGS env var.
                // Each /LIBPATH: entry becomes a `-C link-arg=...` so
                // rustc passes it through to lld-link. Whitespace-
                // separated rustflags is cargo's documented contract.
                let link_args = xwin_msvc_link_args(&cache_dir, target_triple);
                if !link_args.is_empty() {
                    prep.env.push((
                        format!("CARGO_TARGET_{target_u_upper}_RUSTFLAGS"),
                        link_args,
                    ));
                }
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

/// Build the MSVC-style include-flag string that cargo-xwin would
/// have injected for a child cargo invocation. Format:
/// `"/imsvc <crt/include> /imsvc <sdk/include/ucrt> ..."` — each
/// `/imsvc` is followed by the path as a separate token. clang-cl
/// natively recognizes `/imsvc <path>` as an MSVC-style include
/// directive (equivalent to clang's `-isystem <path>`).
///
/// Paths that don't exist on disk are silently skipped — defends
/// against catalogue-shape drift (e.g. a future xwin-cache that
/// stops shipping the winrt include tree shouldn't make the whole
/// CFLAGS injection error out).
fn xwin_msvc_cflags(cache_dir: &std::path::Path) -> String {
    let candidates = [
        cache_dir.join("crt").join("include"),
        cache_dir.join("sdk").join("include").join("ucrt"),
        cache_dir.join("sdk").join("include").join("um"),
        cache_dir.join("sdk").join("include").join("shared"),
        cache_dir.join("sdk").join("include").join("winrt"),
        cache_dir.join("sdk").join("include").join("cppwinrt"),
    ];
    candidates
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| format!("/imsvc {}", p.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the rustc `-C link-arg=/LIBPATH:<path>` chain for the
/// xwin-cache so lld-link finds the MSVC import libs without
/// cargo-xwin being in the loop. Returns a whitespace-separated
/// rustflags string consumable via `CARGO_TARGET_<T>_RUSTFLAGS`.
///
/// The xwin tarball lays libs out per-arch as `crt/lib/<arch>/` and
/// `sdk/lib/{um,ucrt}/<arch>/` where `<arch>` is the MS arch name
/// (`x64`, `arm64`) — matching xwin's `--preserve-ms-arch-notation`
/// flag in the upstream recipe.
fn xwin_msvc_link_args(cache_dir: &std::path::Path, target_triple: &str) -> String {
    let arch = if target_triple.starts_with("aarch64-") {
        "arm64"
    } else if target_triple.starts_with("x86_64-") {
        "x64"
    } else {
        return String::new();
    };
    let candidates = [
        cache_dir.join("crt").join("lib").join(arch),
        cache_dir.join("sdk").join("lib").join("um").join(arch),
        cache_dir.join("sdk").join("lib").join("ucrt").join(arch),
    ];
    candidates
        .iter()
        .filter(|p| p.is_dir())
        .flat_map(|p| {
            vec![
                "-C".to_string(),
                format!("link-arg=/LIBPATH:{}", p.display()),
            ]
        })
        .collect::<Vec<_>>()
        .join(" ")
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

    crate::timed_test!(xwin_cflags_emits_imsvc_for_present_dirs, {
        // soldr#1036: simulate an xwin-cache layout, confirm CFLAGS
        // string contains an `/imsvc <path>` entry for each present
        // include subtree (and skips absent ones).
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        // Materialize crt/include + sdk/include/ucrt only; leave the
        // others absent to prove the filter works.
        std::fs::create_dir_all(root.join("crt").join("include")).unwrap();
        std::fs::create_dir_all(root.join("sdk").join("include").join("ucrt")).unwrap();

        let cflags = xwin_msvc_cflags(root);
        assert!(
            cflags.contains("/imsvc"),
            "cflags must contain /imsvc directive: {cflags}"
        );
        // Both materialized paths should appear, separated by `/imsvc`.
        let imsvc_count = cflags.matches("/imsvc").count();
        assert_eq!(
            imsvc_count, 2,
            "expected 2 /imsvc entries (one per present subtree), got: {cflags}"
        );
        // Absent winrt subtree must NOT have an entry.
        assert!(
            !cflags.contains("winrt"),
            "absent winrt subtree leaked into cflags: {cflags}"
        );
    });

    crate::timed_test!(xwin_cflags_empty_for_empty_cache, {
        // No subtrees present → empty cflags string. Caller can detect
        // this and skip the CFLAGS_<t> env var injection entirely.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cflags = xwin_msvc_cflags(tmp.path());
        assert!(cflags.is_empty(), "expected empty cflags, got: {cflags:?}");
    });

    crate::timed_test!(xwin_link_args_picks_correct_arch_subdir, {
        // Confirm aarch64-pc-windows-msvc looks under `arm64/`,
        // x86_64-pc-windows-msvc looks under `x64/`. This is the
        // MS-arch-notation contract from xwin's
        // --preserve-ms-arch-notation flag in the upstream recipe.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        for arch in ["arm64", "x64"] {
            std::fs::create_dir_all(
                root.join("crt").join("lib").join(arch),
            )
            .unwrap();
            std::fs::create_dir_all(
                root.join("sdk").join("lib").join("um").join(arch),
            )
            .unwrap();
            std::fs::create_dir_all(
                root.join("sdk").join("lib").join("ucrt").join(arch),
            )
            .unwrap();
        }

        let aarch64 = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc");
        assert!(
            aarch64.contains("/arm64") || aarch64.contains("\\arm64"),
            "aarch64 must hit arm64 subdir: {aarch64}"
        );
        assert!(
            !aarch64.contains("/x64") && !aarch64.contains("\\x64"),
            "aarch64 link args leaked x64 path: {aarch64}"
        );

        let x86 = xwin_msvc_link_args(root, "x86_64-pc-windows-msvc");
        assert!(
            x86.contains("/x64") || x86.contains("\\x64"),
            "x86_64 must hit x64 subdir: {x86}"
        );
        assert!(
            !x86.contains("/arm64") && !x86.contains("\\arm64"),
            "x86_64 link args leaked arm64 path: {x86}"
        );
    });

    crate::timed_test!(xwin_link_args_unknown_arch_returns_empty, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        // Non-MSVC triple → empty link args.
        let out = xwin_msvc_link_args(tmp.path(), "x86_64-unknown-linux-gnu");
        assert!(
            out.is_empty(),
            "non-msvc triple must yield empty link args, got: {out:?}"
        );
    });

    crate::timed_test!(xwin_link_args_format_uses_c_link_arg_pairs, {
        // Each /LIBPATH: must be paired with a leading `-C` so rustc
        // parses them as link-args. Without the `-C` prefix the flag
        // would be passed as a plain rustc arg and silently dropped.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("crt").join("lib").join("arm64"))
            .unwrap();

        let out = xwin_msvc_link_args(root, "aarch64-pc-windows-msvc");
        // Count `-C` tokens vs `link-arg=/LIBPATH:` tokens; should be equal.
        let dash_c = out.split_whitespace().filter(|t| *t == "-C").count();
        let link_arg = out
            .split_whitespace()
            .filter(|t| t.starts_with("link-arg=/LIBPATH:"))
            .count();
        assert_eq!(
            dash_c, link_arg,
            "every link-arg must be preceded by -C: {out}"
        );
        assert!(dash_c >= 1, "expected at least one link-arg pair: {out}");
    });
}
