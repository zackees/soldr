//! Argument-parsing and cross-compile dispatch helpers for the `soldr`
//! CLI. Extracted verbatim from `main.rs` (soldr#1368) to keep that file
//! under the 1,500-LOC guard; re-exported at the crate root via
//! `pub(crate) use cli_dispatch::*;` so call sites and the sibling
//! `main_tests.rs` (`use super::*`) resolve these by their original
//! bare names.

use crate::cargo_front_door;
use crate::core::SoldrError;

pub(crate) fn should_self_relocate_for_invocation(raw_args: &[String]) -> bool {
    let user_args = raw_args.get(1..).unwrap_or(&[]);
    let Ok((_, args)) = extract_as_pin(user_args) else {
        return false;
    };

    let mut cache_enabled = true;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--no-cache" => {
                cache_enabled = false;
                idx += 1;
            }
            "--zccache" => {
                // Value lives in the next token; skip both so the
                // subcommand check below lands on `cargo` instead of
                // the value.
                idx += 2;
            }
            "--" => return false,
            arg if arg.starts_with('-') => idx += 1,
            "cargo" => {
                return cache_enabled
                    && cargo_front_door::cargo_args_are_cacheable(&args[idx + 1..])
                    && matches!(
                        crate::zccache::rustc_wrapper_mode(),
                        crate::zccache::RustcWrapperMode::ManagedZccache
                    );
            }
            _ => return false,
        }
    }

    false
}

/// soldr#1012 PR 5 — scan `args` for `--target X` (two-arg form) or
/// `--target=X` (single-arg form). Returns the FIRST occurrence; if
/// both forms are present the single-arg form wins by virtue of
/// appearing first in a left-to-right scan (cargo behavior is
/// "last --target wins", but for prep purposes any match is enough
/// because the prep is target-keyed and cargo handles the final
/// dispatch).
pub(crate) fn extract_target_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(triple) = arg.strip_prefix("--target=") {
            if !triple.is_empty() {
                return Some(triple.to_string());
            }
        }
        if arg == "--target" {
            if let Some(next) = iter.next() {
                if !next.is_empty() {
                    return Some(next.clone());
                }
            }
        }
    }
    None
}

/// soldr#882: pick the cargo subcommand to dispatch for a given
/// cross-target. Only fires on Linux hosts — native macos/windows
/// host builds keep using plain `cargo build`.
///
/// Returns:
/// * `None` for `*-pc-windows-msvc` when blessed prep produced an
///   xwin-cache, so `soldr build` uses plain `cargo build` with the
///   prepared clang/lld/MSVC SDK env.
/// * `Some("xwin")` for `*-pc-windows-msvc` only when
///   `SOLDR_USE_LEGACY_XWIN` explicitly requests the diagnostic path.
/// * `None` for `*-apple-darwin` by default; `Some("zigbuild")` only
///   when `SOLDR_USE_LEGACY_ZIGBUILD` is set for diagnostic comparison.
/// * `None` for `x86_64-pc-windows-gnu`; that target stays on the
///   blessed managed MinGW/GNU path and no longer falls back to
///   cargo-zigbuild
/// * `None` for blessed Linux GNU/musl targets; `Some("zigbuild")` only
///   when `SOLDR_USE_LEGACY_ZIGBUILD` explicitly requests the diagnostic
///   fallback.
/// * `None` for everything else
pub(crate) fn pick_cross_subcommand(
    target_triple: &str,
    _msvc_blessed_cache_ready: bool,
) -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let legacy_xwin = std::env::var_os(crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let legacy_zigbuild = std::env::var_os(crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);

    if target_triple.ends_with("-pc-windows-msvc") {
        return if legacy_xwin { Some("xwin") } else { None };
    }
    // soldr#1081 follow-up: `*-apple-darwin` no longer routes through
    // cargo-zigbuild. The blessed-build apple-darwin arm in
    // `blessed_build.rs` now exports the COMPLETE Apple SDK to cc-rs +
    // rustc's linker, so plain `cargo build --target X` produces a
    // Mach-O binary from a Linux host without the
    // tikv-jemalloc-sys/zig-minimal-sysroot mismatch that broke the
    // release lane. `SOLDR_USE_LEGACY_ZIGBUILD=1` re-routes darwin
    // through zigbuild for diagnostic comparison.
    if target_triple.ends_with("-apple-darwin") {
        return if legacy_zigbuild {
            Some("zigbuild")
        } else {
            None
        };
    }
    if target_triple.ends_with("-unknown-linux-musl") {
        return if legacy_zigbuild {
            Some("zigbuild")
        } else {
            None
        };
    }
    // Cross from x86_64 host to aarch64 linux — needs zigbuild for
    // the bundled libc.
    if target_triple == "aarch64-unknown-linux-gnu" && cfg!(target_arch = "x86_64") {
        return if legacy_zigbuild {
            Some("zigbuild")
        } else {
            None
        };
    }
    None
}

/// soldr#882: rewrite the args vector for the picked cargo subcommand.
///
/// For `zigbuild`: cargo-zigbuild IS the build verb — replace the
/// leading `build` with `zigbuild`. So `["build", "--target", X, ...]`
/// becomes `["zigbuild", "--target", X, ...]`.
///
/// For `xwin`: cargo-xwin uses `xwin build ...` as a subcommand
/// pair — prepend `xwin` keeping the `build` verb. So
/// `["build", "--target", X, ...]` becomes
/// `["xwin", "build", "--target", X, ...]`.
pub(crate) fn rewrite_build_args_for_subcommand(
    mut args: Vec<String>,
    subcmd: &str,
) -> Vec<String> {
    match subcmd {
        "zigbuild" => {
            if let Some(first) = args.first_mut() {
                if first == "build" {
                    *first = "zigbuild".to_string();
                }
            }
            args
        }
        "xwin" => {
            args.insert(0, "xwin".to_string());
            args
        }
        _ => args,
    }
}

pub(crate) fn insert_cargo_config_args(
    mut args: Vec<String>,
    cargo_config_args: &[String],
) -> Vec<String> {
    if cargo_config_args.is_empty() {
        return args;
    }

    let insert_at = if args.first().is_some_and(|arg| arg == "xwin")
        && args.get(1).is_some_and(|arg| arg == "build")
    {
        2
    } else if args.is_empty() {
        0
    } else {
        1
    };
    args.splice(insert_at..insert_at, cargo_config_args.iter().cloned());
    args
}

/// soldr#1079 — bridge between the cargo dispatcher and the MSVC
/// host-discovery module. Resolves the target triple (explicit
/// `--target` first, then `TargetTriple::detect()` for the implicit
/// native default) and asks [`crate::msvc_host`] to inject the
/// vcvars-equivalent env vars when relevant.
///
/// All branches are silent on success / no-op. Discovery errors are
/// printed as a single warning line so the user gets a hint about
/// `SOLDR_MSVC_DISCOVERY=off` if the auto-probe trips on an unusual
/// install — the underlying cargo invocation still runs and emits
/// its own (better) error if it actually needs the env.
pub(crate) fn ensure_msvc_host_env_for_native(args: &[String]) {
    if !cfg!(target_os = "windows") {
        return;
    }
    let target = match extract_target_from_args(args) {
        Some(t) => t,
        None => match crate::core::TargetTriple::detect() {
            Ok(t) => t.triple(),
            Err(_) => return,
        },
    };
    match crate::msvc_host::ensure_msvc_env_for_native(&target) {
        Ok(true) => {
            tracing::debug!(
                target: "soldr::msvc_host",
                target_triple = %target,
                "injected host MSVC env (LIB/INCLUDE/PATH/LIBPATH) for native build"
            );
        }
        Ok(false) => {
            // Skipped — non-windows, non-msvc, opt-out, or already-set.
            // Nothing to log; this is the steady-state branch in dev
            // command prompts.
        }
        Err(err) => {
            eprintln!(
                "soldr: MSVC host discovery failed for {target}: {err}\n\
                 soldr: cargo will still run; set SOLDR_MSVC_DISCOVERY=off to silence this probe."
            );
        }
    }
}

/// soldr#1012 PR 5 — prepend `dir` to the current process's `PATH`
/// env var. Idempotent in the sense that if `dir` is already first
/// on PATH, the value is unchanged (PATH stays clean).
pub(crate) fn prepend_to_path_env(dir: &std::path::Path) {
    let dir = dir.to_path_buf();
    prepend_path_dirs_to_env(std::slice::from_ref(&dir));
}

/// Prepend a complete PATH prefix without reversing its priority. Repeatedly
/// prepending individual entries would turn `[shim, llvm, cmake]` into
/// `[cmake, llvm, shim]`, which lets managed clang bypass the MSVC shim.
pub(crate) fn prepend_path_dirs_to_env(dirs: &[std::path::PathBuf]) {
    if dirs.is_empty() {
        return;
    }
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut existing: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    if existing.starts_with(dirs) {
        return;
    }
    let mut combined = Vec::with_capacity(dirs.len() + existing.len());
    combined.extend_from_slice(dirs);
    combined.append(&mut existing);
    if let Ok(joined) = std::env::join_paths(combined) {
        std::env::set_var("PATH", joined);
    }
}

pub(crate) fn extract_as_pin(args: &[String]) -> Result<(Option<String>, Vec<String>), SoldrError> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut version: Option<String> = None;
    let mut before_subcommand = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !before_subcommand {
            out.push(arg.clone());
            continue;
        }
        if arg == "--as" {
            let value = iter.next().ok_or_else(|| {
                SoldrError::Other("--as requires a version argument, e.g. --as 0.5.2".into())
            })?;
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as version argument must not be empty".into(),
                ));
            }
            version = Some(value.clone());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--as=") {
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as= requires a version, e.g. --as=0.5.2".into(),
                ));
            }
            version = Some(value.to_string());
            continue;
        }
        if arg == "--" {
            before_subcommand = false;
            out.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        before_subcommand = false;
        out.push(arg.clone());
    }
    Ok((version, out))
}

/// True when the requested version is different from this binary's. A match
/// short-circuits the trampoline so the current in-process soldr handles it.
pub(crate) fn should_trampoline(requested: &str) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    normalize_version(requested) != normalize_version(current)
}

pub(crate) fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}
