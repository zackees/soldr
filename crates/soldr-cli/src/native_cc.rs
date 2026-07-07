//! Native C/C++ compiler caching via zccache (issue #310).
//!
//! Wires `CC` / `CXX` (and target-specific `CC_<triple>` / `CXX_<triple>`)
//! through zccache wrapper-mode so cc-rs invocations from cargo build
//! scripts hit the same managed cache that rustc invocations do. Without
//! this, fixtures like `rusqlite[bundled]` (which calls cc-rs from
//! `libsqlite3-sys`'s `build.rs` to compile ~50 C source files) bypass
//! zccache entirely and force a full C recompile on every warm build —
//! the regression behind soldr#491.
//!
//! zccache already accepts the wrapper-mode invocation `zccache <compiler>
//! <args>` (see `compiler/detect.rs` in zackees/zccache — gcc, clang, and
//! MSVC are first-class families). The soldr-side change is just env-var
//! plumbing: prepend the zccache binary path to the user's chosen C
//! compiler so cc-rs spawns `zccache cc -c foo.c -o foo.o` instead of
//! `cc -c foo.c -o foo.o`.
//!
//! ## Default-on with safe escape hatches
//!
//! Native caching is **default-on** when:
//! 1. `soldr --no-cache cargo …` is NOT set (the global cache kill-switch).
//! 2. `SOLDR_NATIVE_CACHE` is unset or set to a truthy value.
//! 3. zccache wrapper mode is the resolved `RUSTC_WRAPPER` choice (i.e.
//!    not `SOLDR_RUSTC_WRAPPER=sccache` or `SOLDR_RUSTC_WRAPPER=disabled`).
//!
//! Opt-outs:
//! - `SOLDR_NATIVE_CACHE=0` / `false` / `off` — disables only the native
//!   compiler wrapping; rustc-side zccache caching stays on.
//! - `soldr --no-cache cargo …` — disables both.
//!
//! ## No-double-wrap
//!
//! When the user has already set `CC="sccache clang"` (or ccache /
//! cachepot / zccache itself), we leave the existing value untouched.
//! Re-wrapping would yield `CC="zccache sccache clang"`, which sccache
//! doesn't recognize. The known-wrapper set is intentionally narrow so
//! a future wrapper not on the list falls through to the wrap path and
//! the user can opt out via `SOLDR_NATIVE_CACHE=0` if needed.
//!
//! ## Windows scope
//!
//! v1 wraps **user-explicit** `CC` / `CXX` on every platform, but only
//! synthesises a default (bare `cc` / `c++`) on Unix. On Windows we don't
//! inject a default because there's no canonical `cc` on PATH — cc-rs
//! does vcvars autodetection to find `cl.exe`, and clobbering that with
//! `CC="zccache cc"` would break builds. Windows users who want the
//! native-cache benefit set `CC=cl.exe` (or `CC=clang-cl`) explicitly,
//! and the wrapper kicks in. The opt-out via `SOLDR_NATIVE_CACHE=0`
//! works the same on every platform.

use crate::core::SoldrError;
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Env-var that disables only the native-cache injection. `SOLDR_NATIVE_CACHE=0`
/// (or `false` / `off` / `no`) skips this module; rustc-side zccache caching
/// stays on. Truthy values (`1`, `true`, `yes`, any other non-empty string)
/// keep the default-on behaviour.
pub(crate) const NATIVE_CACHE_ENV_VAR: &str = "SOLDR_NATIVE_CACHE";

/// cc-rs honors this list to recognize wrapper-style compilers without a
/// hardcoded match against its built-in known-wrapper set. Setting it
/// equal to `zccache` (the basename) lets cc-rs strip the wrapper and
/// detect the real compiler underneath — without this, cc-rs can mis-
/// identify our wrapped `CC="zccache cc"` as the compiler itself.
const CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR: &str = "CC_KNOWN_WRAPPER_CUSTOM";

/// Basenames of compiler-cache wrappers we recognize. Used to avoid
/// double-wrapping a `CC="<wrapper> <real-compiler>"` value that the
/// user (or another tool) already set.
const KNOWN_WRAPPERS: &[&str] = &["zccache", "zccache-soldr", "sccache", "ccache", "cachepot"];

/// Outcome of evaluating the opt-outs. The variants drive what the
/// caller does: `Inject` runs the full wrapping pass, including the
/// synthesised default for unset `CC` / `CXX`; `InjectExistingOnly`
/// wraps user-set values but leaves unset vars alone (Windows
/// default); `UserDisabled` skips everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCacheDecision {
    /// Inject wrapping for set + unset compiler env vars.
    Inject,
    /// Inject wrapping only when the user already set the env var —
    /// don't synthesize a default. Windows: cc-rs's vcvars autodetection
    /// must keep working when CC is unset, so we never inject a default.
    InjectExistingOnly,
    /// User opted out via `SOLDR_NATIVE_CACHE`. Don't inject.
    UserDisabled,
}

/// Compute the decision for the current process. Pure: only inspects
/// the env var and the compile-time host platform.
pub(crate) fn native_cache_decision() -> NativeCacheDecision {
    if env_is_falsy(std::env::var_os(NATIVE_CACHE_ENV_VAR).as_deref()) {
        return NativeCacheDecision::UserDisabled;
    }
    if cfg!(target_os = "windows") {
        return NativeCacheDecision::InjectExistingOnly;
    }
    NativeCacheDecision::Inject
}

/// True when the shim binary is reachable — either as an existing file
/// at the resolved path, or as a bare basename that can plausibly be
/// found via PATH lookup. Production callers materialize
/// `zccache-soldr` from the main `soldr` binary first; the bare-basename
/// branch keeps unit tests and unusual external callers defensive.
fn shim_is_available(shim: &Path) -> bool {
    if shim.is_file() {
        return true;
    }
    let is_bare_basename = shim
        .file_name()
        .map(|n| Path::new(n) == shim)
        .unwrap_or(false);
    if !is_bare_basename {
        return false;
    }
    let Some(basename) = shim.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|dir| {
        let candidate = dir.join(basename);
        candidate.is_file()
    })
}

/// True when the env value resolves to "the user opted out". Empty /
/// missing returns false (default-on). The set of opt-out strings
/// mirrors the soldr convention used by `SOLDR_NO_GC_TARGET` etc.
fn env_is_falsy(value: Option<&OsStr>) -> bool {
    let Some(raw) = value else { return false };
    let Some(s) = raw.to_str() else { return false };
    let t = s.trim();
    matches!(
        t.to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Inject `CC` / `CXX` (and matching target-specific variants if the
/// user has set them) so cc-rs invocations run `<zccache> <real-cc>`
/// instead of `<real-cc>` directly. Idempotent: re-running with already-
/// wrapped values is a no-op.
///
/// `wrapper_shim` must be the path to the `zccache-soldr` shim
/// (soldr#1368) — the stable RUSTC_WRAPPER/CC shim that forwards a
/// compile to the soldr-daemon embedded zccache service over the
/// `Request::Compile` IPC verb. cc-rs will spawn
/// `zccache-soldr <abs-compiler> <args>`; the shim strips its own argv[0]
/// and dispatches `[<abs-compiler>, ...args]` to the daemon, which detects
/// the C/C++ compiler family by basename.
///
/// The underlying compiler is resolved to an ABSOLUTE path here because
/// the daemon invokes it with `Command::new(compiler)` under an
/// env_clear + replay, so a bare `cc` on the child's PATH is not reliably
/// found.
///
/// `target` is the active cargo build target (`x86_64-unknown-linux-gnu`,
/// etc.) when known; passed through to discover target-specific
/// `CC_<triple>` / `CXX_<triple>` values cc-rs honors.
pub(crate) fn inject_native_cache_env(
    cargo: &mut std::process::Command,
    wrapper_shim: &Path,
    target: Option<&str>,
) -> Result<(), SoldrError> {
    let decision = native_cache_decision();
    let synthesize_default = match decision {
        NativeCacheDecision::Inject => true,
        NativeCacheDecision::InjectExistingOnly => false,
        NativeCacheDecision::UserDisabled => return Ok(()),
    };

    // soldr#1374 / #1368 followup: skip injection when the wrapper path
    // is not actually spawnable. Native-C caching is a perf enhancement,
    // not a correctness requirement.
    if !shim_is_available(wrapper_shim) {
        return Ok(());
    }

    let wrapper = wrapper_shim.as_os_str().to_os_string();

    // Generic CC / CXX.
    for (var, default_compiler) in [("CC", "cc"), ("CXX", "c++")] {
        if let Some(wrapped) =
            wrap_compiler_env(var, default_compiler, &wrapper, synthesize_default)
        {
            cargo.env(var, wrapped);
        }
    }

    // Target-specific variants. cc-rs 1.2.x reads these in order:
    //   1. `<TOOL>_<target-with-hyphens>`
    //   2. `<TOOL>_<target-as-snake>`
    //   3. `TARGET_<TOOL>` / `HOST_<TOOL>`
    //   4. `<TOOL>` (fallback)
    //
    // Most shells cannot conveniently export names with hyphens, so soldr's
    // own prep layers historically set only the snake form. Mirror the same
    // wrapped compiler into both spellings so cc-rs's first lookup key does
    // not fall through to generic `CC`.
    if let Some(triple) = target {
        for (prefix, default_compiler) in [
            ("CC_", default_c_compiler_for_target(triple)),
            ("CXX_", default_cxx_compiler_for_target(triple)),
        ] {
            wrap_target_compiler_envs(
                cargo,
                prefix,
                triple,
                default_compiler,
                &wrapper,
                synthesize_default,
            );
        }
    }

    // Let cc-rs strip the wrapper when classifying the real compiler
    // beneath it. Without this, cc-rs may decide that
    // `CC="zccache-soldr cc"` names a compiler called `zccache-soldr`,
    // not a wrapper. The value must be the wrapper's file stem. Honor an
    // existing user value — they may already have CC_KNOWN_WRAPPER_CUSTOM
    // set to a different wrapper (sccache, etc.) and we don't want to
    // clobber it.
    if std::env::var_os(CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR).is_none() {
        let stem = wrapper_shim
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("zccache-soldr");
        cargo.env(CC_KNOWN_WRAPPER_CUSTOM_ENV_VAR, stem);
    }

    Ok(())
}

fn wrap_target_compiler_envs(
    cargo: &mut std::process::Command,
    prefix: &str,
    triple: &str,
    default_compiler: &str,
    wrapper: &OsStr,
    synthesize_default: bool,
) {
    let dashed = format!("{prefix}{triple}");
    let snake = format!("{prefix}{}", triple.replace(['-', '.'], "_"));
    let source = if std::env::var_os(&dashed).is_some() {
        dashed.as_str()
    } else {
        snake.as_str()
    };
    let existing = command_env_value(cargo, &dashed)
        .or_else(|| command_env_value(cargo, &snake))
        .or_else(|| std::env::var_os(source));

    if let Some(wrapped) =
        wrap_compiler_value(existing, default_compiler, wrapper, synthesize_default)
    {
        cargo.env(&snake, &wrapped);
        if dashed != snake {
            cargo.env(&dashed, wrapped);
        }
    }
}

fn command_env_value(command: &std::process::Command, key: &str) -> Option<OsString> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .and_then(|(_, value)| value.map(OsString::from))
}

/// Resolve a bare compiler name (`cc`, `c++`, `musl-gcc`, …) to an
/// absolute path via a PATH lookup in the current process (soldr#1368).
/// The daemon spawns the compiler with `Command::new(compiler)` under an
/// env_clear + replay, so bare names are not reliably resolved on the
/// child side — resolve here where soldr's own PATH still holds. Returns
/// the input unchanged when it is already absolute or cannot be found
/// (best-effort; the daemon still gets a well-formed request).
fn resolve_compiler_to_abs(name: &str) -> OsString {
    let candidate = Path::new(name);
    if candidate.is_absolute() {
        return OsString::from(name);
    }
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".bat", ".cmd"]
    } else {
        &[""]
    };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for ext in exts {
                let file = if ext.is_empty() {
                    dir.join(name)
                } else {
                    dir.join(format!("{name}{ext}"))
                };
                if file.is_file() {
                    return file.into_os_string();
                }
            }
        }
    }
    OsString::from(name)
}

fn default_c_compiler_for_target(triple: &str) -> &'static str {
    if triple.contains("-linux-musl") {
        "musl-gcc"
    } else {
        "cc"
    }
}

fn default_cxx_compiler_for_target(triple: &str) -> &'static str {
    if triple.contains("-linux-musl") {
        "musl-g++"
    } else {
        "c++"
    }
}

/// Compute the wrapped value for one compiler env var. Returns `None`
/// when no change is needed (already wrapped by a known wrapper, or
/// already wrapped by us). Returns `Some(value)` otherwise — including
/// the synthesized default when the env var was unset.
///
/// `default_compiler` is the bare compiler basename to use when the
/// env var is unset (e.g. `cc`, `c++`). Resolution of those bare names
/// to absolute paths is cc-rs's job at compile time; we deliberately
/// don't pin them here so the user's PATH still wins.
fn wrap_compiler_env(
    var: &str,
    default_compiler: &str,
    wrapper: &OsStr,
    synthesize_default: bool,
) -> Option<OsString> {
    wrap_compiler_value(
        std::env::var_os(var),
        default_compiler,
        wrapper,
        synthesize_default,
    )
}

fn wrap_compiler_value(
    existing: Option<OsString>,
    default_compiler: &str,
    wrapper: &OsStr,
    synthesize_default: bool,
) -> Option<OsString> {
    // Decide what the underlying compiler is. Two cases:
    //   - var is unset → use the bare default (`cc` / `c++`) when the
    //     caller asked us to synthesize one; otherwise skip and let
    //     cc-rs's own discovery handle it (the Windows path).
    //   - var is set → respect it, unless it's already wrapped.
    let underlying = match existing {
        Some(raw) => {
            // Trim leading whitespace; cc-rs splits on whitespace
            // anyway so any extra is harmless, but we want a clean
            // value to test against.
            let s = raw.to_string_lossy().trim().to_string();
            if s.is_empty() {
                if !synthesize_default {
                    return None;
                }
                default_compiler.to_string()
            } else if value_starts_with_known_wrapper(&s) {
                // Already wrapped by some wrapper we recognize. Don't
                // double-wrap. Caller treats `None` as "no env change".
                return None;
            } else {
                s
            }
        }
        None => {
            if !synthesize_default {
                return None;
            }
            default_compiler.to_string()
        }
    };

    // Build `<wrapper> <abs-underlying>`. Use OsString throughout so we
    // don't lose non-UTF8 bytes from the wrapper path on Linux/macOS.
    // The underlying compiler is resolved to an absolute path so the
    // daemon's `Command::new(compiler)` (env_clear + replay) finds it.
    let mut wrapped = OsString::new();
    wrapped.push(wrapper);
    wrapped.push(" ");
    wrapped.push(resolve_compiler_to_abs(&underlying));
    Some(wrapped)
}

/// True when the first whitespace-tokenized word of `value` names a
/// compiler-cache wrapper we recognize. Strips a `.exe` extension and
/// drops the directory component so `/usr/local/bin/sccache` and
/// `C:\Tools\sccache.exe` both match.
///
/// Path splitting is done manually on both `/` and `\` separators
/// regardless of host OS — soldr may see Windows-style paths in `CC`
/// from cross-builds even on Linux runners, and the Rust standard
/// library's `Path` only recognizes the host's native separator.
fn value_starts_with_known_wrapper(value: &str) -> bool {
    let first = match value.split_whitespace().next() {
        Some(t) => t,
        None => return false,
    };
    let basename = first.rsplit(['/', '\\']).next().unwrap_or(first);
    let stem = match basename.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem,
        _ => basename,
    };
    KNOWN_WRAPPERS.iter().any(|w| stem.eq_ignore_ascii_case(w))
}

#[cfg(test)]
#[path = "native_cc_tests.rs"]
mod tests;
