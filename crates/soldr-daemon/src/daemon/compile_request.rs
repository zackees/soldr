//! Shared argv → `CompileRequest` parser (soldr#2388 Step 6 / #2365 Q2).
//!
//! fable5's ruling: a SESSION `SessionStart` carries the raw rustc argv + env +
//! cwd, and the argv→`CompileRequest` conversion lives daemon-side in **one**
//! shared function so the `RUSTC_WRAPPER` client and the daemon's SESSION
//! codec-bridge can never disagree on parsing.
//!
//! - The wrapper reads its own `std::env` cwd/vars and calls
//!   [`build_compile_request_from`] (see `soldr_cli::compile_dispatch`).
//! - The SESSION endpoint (Step 6) passes the `SessionStart`'s carried cwd/env,
//!   never the daemon's own process env.
//!
//! The env is filtered by [`is_compile_env_var`] exactly as the legacy wrapper
//! path — the surface is never widened (fable5 Q2 condition 1). The filter is
//! idempotent, so passing an already-filtered env is safe.

use std::time::SystemTime;

use crate::daemon::protocol::{CompileLifecycle, CompileRequest};

/// Build a [`CompileRequest`] from a rustc-style argv (`argv[0]` is the rustc
/// path, `argv[1..]` the rustc arguments) plus an explicit `cwd` and `env`.
///
/// Taking `cwd`/`env` explicitly is what lets the daemon build a request from a
/// `SessionStart`'s carried environment rather than its own. `env` is filtered
/// by [`is_compile_env_var`]; pass the raw environment.
pub fn build_compile_request_from(
    rustc_argv: &[String],
    cwd: String,
    env: impl IntoIterator<Item = (String, String)>,
) -> CompileRequest {
    let env: Vec<(String, String)> = env
        .into_iter()
        .filter(|(k, _)| is_compile_env_var(k))
        .collect();
    let lifecycle = build_compile_lifecycle_from(rustc_argv, &env);
    CompileRequest {
        args: rustc_argv.to_vec(),
        cwd,
        env,
        stdin: Vec::new(),
        lifecycle,
        ipc_busy_retries: 0,
    }
}

/// Derive the build-history lifecycle from the (already-filtered) env — the
/// build-session id survives the env filter, so this reads it from `env` rather
/// than `std::env`, keeping the daemon path honest to the SessionStart.
fn build_compile_lifecycle_from(
    rustc_argv: &[String],
    env: &[(String, String)],
) -> Option<CompileLifecycle> {
    let session_id = env
        .iter()
        .find(|(k, _)| k == soldr_cache::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)?
        .1
        .parse::<u64>()
        .ok()?;
    let rustc_args = rustc_argv.get(1..).unwrap_or_default();
    let target_dir =
        soldr_cache::cache_lib::target_registry::resolve_workspace_target_dir(rustc_args)?;
    Some(CompileLifecycle {
        session_id,
        crate_name: parse_crate_name(rustc_args)
            .unwrap_or("unknown")
            .to_string(),
        target_dir: target_dir.display().to_string(),
        started_at_ms: current_unix_ms(),
    })
}

fn parse_crate_name(args: &[String]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--crate-name" {
            return args.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--crate-name=") {
            return Some(value);
        }
    }
    None
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Whether an environment variable is forwarded on the compile request.
///
/// A small denylist strips desktop/login-session and shell-prompt plumbing so
/// session noise never crosses the wire; everything else — including vars we
/// have never heard of — is forwarded, matching the standalone zccache wrapper
/// client's whole-env forwarding. zccache's fingerprint only hashes `CARGO_*`
/// vars, so the extra forwarded vars do not churn cache keys.
pub fn is_compile_env_var(name: &str) -> bool {
    // Desktop / login-session plumbing. Prefix matches.
    const NOISE_PREFIXES: &[&str] = &[
        "XDG_",     // XDG_SESSION_TYPE, XDG_RUNTIME_DIR, ...
        "GNOME_",   // GNOME_TERMINAL_SERVICE, ...
        "GDM",      // GDM_LANG, GDMSESSION
        "DESKTOP_", // DESKTOP_SESSION-adjacent
    ];
    if NOISE_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // Shell-prompt / terminal-session state. Exact matches.
    !matches!(
        name,
        "PROMPT"
            | "PS1"
            | "PS2"
            | "PS4"
            | "OLDPWD"
            | "SHLVL"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "SESSION_MANAGER"
            | "LS_COLORS"
            | "LSCOLORS"
            | "PSModulePath"
            | "ChocolateyInstall"
            | "ChocolateyLastPathUpdate"
            | "WSL_DISTRO_NAME"
            | "WSL_INTEROP"
            | "WT_SESSION"
            | "WT_PROFILE_ID"
            | "WINDOWID"
            | "COLORTERM"
            | "VTE_VERSION"
    )
}
