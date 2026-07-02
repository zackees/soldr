//! Shared `Request::Compile` dispatch logic (issue #1081).
//!
//! Lifted out of `wrapper.rs` so it is reachable by both the existing
//! soldr-as-wrapper hot path AND the dedicated `zccache-soldr` shim
//! binary added in #1081. Both callers do the same thing:
//!
//! 1. Build a `CompileRequest` from the rustc-style argv they were
//!    invoked with.
//! 2. Forward the current process env minus known session noise
//!    (see [`is_compile_env_var`]) — build-script-emitted
//!    `cargo:rustc-env` vars have arbitrary names and MUST survive.
//! 3. Connect to the soldr-daemon IPC socket / Windows named pipe.
//! 4. On failure to connect, spawn the daemon detached and retry
//!    within a configurable budget (default 30s for production —
//!    `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` overrides for tests).
//! 5. Stream the `CompileStdoutChunk` / `CompileStderrChunk` frames
//!    back to the caller's stdout/stderr until `CompileDone` arrives,
//!    then return the captured exit code.
//!
//! ## Hang-safety contract (issue #1081)
//!
//! Every IPC call has an explicit per-call timeout. The spawn-retry
//! loop has a hard wall-clock budget (no "wait forever for daemon to
//! come up"). The retry budget defaults to 30 s in production and is
//! overridable via `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` so tests can
//! force a sub-second fast-fail and never wedge the test runner.
//!
//! The daemon-side already drops the inflight compile if the wrapper
//! disconnects (see PR #1078 / `daemon::server::race_against_disconnect`),
//! so a ctrl-c in the shim or wrapper cancels the embedded rustc
//! invocation cleanly.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::client;
use crate::daemon::protocol::CompileRequest;

/// Env var that overrides the spawn-retry budget in milliseconds.
/// Default production budget is `DEFAULT_SPAWN_RETRY_BUDGET_MS`; tests
/// set this to a sub-second value so the wait-for-daemon loop fails
/// fast against a known-absent socket instead of wedging the test
/// runner.
pub const SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR: &str = "SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS";

/// Default 30-second budget for the daemon-spawn + retry loop.
///
/// Embedded zccache cold-start (redb open, cache root init, depgraph
/// load) can take several seconds on a first-ever boot in a fresh
/// container — this budget needs to comfortably cover that worst case.
pub const DEFAULT_SPAWN_RETRY_BUDGET_MS: u64 = 30_000;

/// Per-attempt retry interval — keeps the loop cheap when the daemon
/// is coming up but doesn't waste an extra ~hundred-ms after the
/// socket starts accepting.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Predicate for the env-var filter (Phase 5c from #981, reworked for
/// correctness after the `cargo:rustc-env` regression that broke the
/// linux-arm-musl cross-compile lane — run 28574600982).
///
/// Returns `true` for env vars that must be forwarded in the
/// `Request::Compile` payload so the daemon-spawned rustc sees the
/// same environment cargo gave the wrapper process.
///
/// ## Why this is a noise DENYLIST, not an allowlist
///
/// Cargo forwards `cargo:rustc-env=<NAME>=<value>` lines emitted by a
/// crate's build script as environment variables **on the rustc
/// invocation only** — and `<NAME>` is arbitrary (crgx sets
/// `CRGX_TARGET`, vergen sets `VERGEN_*`, shadow-rs sets `SHADOW_*`,
/// ...). The crate then reads them back with `env!()` at compile
/// time. An allowlist can never enumerate those names, and dropping
/// one turns into a hard `error: environment variable ... not defined
/// at compile time` inside the daemon-spawned rustc. That is exactly
/// what broke the `linux-arm-musl` cross-compile lane: the crgx
/// source-build fallback died on `env!("CRGX_TARGET")` because the
/// old allowlist filtered the var out of the compile request.
///
/// So the filter now drops only *known* interactive-session noise
/// (shell prompt state, desktop-session plumbing) that no compiler,
/// build script, or proc-macro legitimately reads. Everything else —
/// including vars we have never heard of — is forwarded. The
/// standalone zccache wrapper client forwards its **entire** env
/// (see `_vender/zccache/crates/zccache/src/cli/commands/wrap/env.rs`),
/// so this keeps the embedded-daemon path consistent with the managed
/// binary path. The Phase 5c payload-size concern is preserved by the
/// denylist (session noise still never crosses the wire) and by the
/// fact that zccache's fingerprint only hashes `CARGO_*` vars, so the
/// extra forwarded vars do not churn cache keys.
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

/// Build a `CompileRequest` from a rustc-style argv. `argv[0]` is the
/// rustc path (or clippy-driver, etc.) and `argv[1..]` are the
/// arguments rustc receives. This is the SAME shape both
/// soldr-as-RUSTC_WRAPPER and the dedicated `zccache-soldr` shim are
/// invoked with — RUSTC_WRAPPER's contract is `[wrapper, rustc_path,
/// ...rustc_args]`, and after the wrapper-binary entry has stripped
/// argv[0] we get the [rustc_path, ...rustc_args] shape this function
/// expects.
pub fn build_compile_request(rustc_argv: &[String]) -> CompileRequest {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| is_compile_env_var(k))
        .collect();
    CompileRequest {
        args: rustc_argv.to_vec(),
        cwd,
        env,
        stdin: Vec::new(),
    }
}

/// Read the spawn-retry budget from env (overridable for tests) or
/// fall back to the production default. Clamped to at least 100 ms so
/// no caller can accidentally disable retries entirely.
pub fn resolved_spawn_retry_budget() -> Duration {
    let ms = std::env::var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SPAWN_RETRY_BUDGET_MS)
        .max(100);
    Duration::from_millis(ms)
}

/// Dispatch a rustc-style compile to soldr-daemon's embedded zccache
/// service and stream the reply back to `stdout`/`stderr`. Returns
/// the rustc exit code on success.
///
/// `rustc_argv` is the [rustc_path, ...rustc_args] tail described in
/// [`build_compile_request`].
///
/// Retry behavior: first attempt uses the current process's
/// daemon-connect timeout. On any failure, spawn the daemon detached
/// and retry every [`RETRY_INTERVAL`] until either a successful
/// `CompileDone` arrives OR the wall-clock budget (per
/// [`resolved_spawn_retry_budget`]) elapses. **The function never
/// blocks forever** — that's the hang-safety contract.
pub fn dispatch_compile<O, E>(
    rustc_argv: &[String],
    mut stdout: O,
    mut stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    let paths =
        SoldrPaths::new().map_err(|e| SoldrError::Other(format!("resolve soldr paths: {e}")))?;
    let sock = client::default_sock_path(&paths);
    let req = build_compile_request(rustc_argv);

    // First try — daemon may already be running.
    if let Ok(done) = client::compile_streaming(&sock, req.clone(), &mut stdout, &mut stderr) {
        return Ok(done.exit_code);
    }

    // Daemon down — spawn detached and retry within budget.
    let _spawn_result = crate::daemon::lifecycle::try_spawn_detached();
    let budget = resolved_spawn_retry_budget();
    let start = Instant::now();
    let mut last_err: Option<client::ClientError> = None;
    while start.elapsed() < budget {
        std::thread::sleep(RETRY_INTERVAL);
        match client::compile_streaming(&sock, req.clone(), &mut stdout, &mut stderr) {
            Ok(done) => return Ok(done.exit_code),
            Err(e) => last_err = Some(e),
        }
    }
    Err(SoldrError::Other(format!(
        "soldr daemon embedded compile dispatch failed after {}ms budget: last_err={:?} sock={}",
        budget.as_millis(),
        last_err,
        sock.display()
    )))
}

/// Variant of [`dispatch_compile`] that takes an explicit socket path
/// override. Lets tests point the dispatch at a known-bad path
/// (non-existent socket) so the retry loop fails fast against the
/// configured budget — proves the no-hang contract without spinning
/// up a real daemon.
pub fn dispatch_compile_with_sock<O, E>(
    sock_path: &Path,
    rustc_argv: &[String],
    mut stdout: O,
    mut stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    let req = build_compile_request(rustc_argv);

    // First try without spawning — we only spawn if a stock socket path
    // is in play; for explicit-override tests, just retry within budget.
    if let Ok(done) = client::compile_streaming(sock_path, req.clone(), &mut stdout, &mut stderr) {
        return Ok(done.exit_code);
    }

    let budget = resolved_spawn_retry_budget();
    let start = Instant::now();
    let mut last_err: Option<client::ClientError> = None;
    while start.elapsed() < budget {
        std::thread::sleep(RETRY_INTERVAL);
        match client::compile_streaming(sock_path, req.clone(), &mut stdout, &mut stderr) {
            Ok(done) => return Ok(done.exit_code),
            Err(e) => last_err = Some(e),
        }
    }
    Err(SoldrError::Other(format!(
        "soldr daemon embedded compile dispatch failed after {}ms budget: last_err={:?} sock={}",
        budget.as_millis(),
        last_err,
        sock_path.display()
    )))
}

/// Wrapper around [`client::compile_streaming`] that mirrors the
/// signature wrapper.rs used to call directly. Re-export with this
/// name keeps the bin-side `compile_via_daemon` callers backward-
/// compatible after the lift.
pub fn compile_via_daemon(rustc_argv: &[String]) -> Result<i32, SoldrError> {
    dispatch_compile(rustc_argv, std::io::stdout(), std::io::stderr())
}

// Re-export the daemon-side type so callers don't have to reach into
// `daemon::client` directly. Useful for the shim's optional logging.
pub use client::CompileDoneInfo as DispatchInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serializes the env-mutating tests in this module so they don't
    /// race the shared `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` env var.
    /// Lower-overhead than pulling in `serial_test` as a dep; the
    /// affected tests all complete in single-digit ms once they hold
    /// the lock.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Lock-guard helper that also saves + restores the budget env
    /// var so tests leave the global env in the state they found it.
    struct BudgetEnvGuard<'a> {
        _guard: std::sync::MutexGuard<'a, ()>,
        prior: Option<std::ffi::OsString>,
    }

    impl<'a> BudgetEnvGuard<'a> {
        fn acquire() -> Self {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR);
            std::env::remove_var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR);
            Self { _guard, prior }
        }

        fn set(&self, value: &str) {
            std::env::set_var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR, value);
        }
    }

    impl<'a> Drop for BudgetEnvGuard<'a> {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR, v),
                None => std::env::remove_var(SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS_ENV_VAR),
            }
        }
    }

    timed_test!(
        is_compile_env_var_recognizes_msvc_link_vars,
        Duration::from_secs(5),
        {
            // Issue #1079 — the MSVC discovery sets LIB / INCLUDE / LIBPATH
            // on the wrapper process. The dispatch's env filter must
            // forward them to the daemon so the embedded rustc invocation
            // gets the same env. Regression-test that explicitly.
            for v in ["LIB", "INCLUDE", "LIBPATH", "PATH"] {
                assert!(is_compile_env_var(v), "{v} must be forwarded to daemon");
            }
        }
    );

    timed_test!(
        is_compile_env_var_forwards_build_script_rustc_env_vars,
        Duration::from_secs(5),
        {
            // Regression test for the linux-arm-musl cross-compile lane
            // failure (run 28574600982): crgx's build.rs emits
            // `cargo:rustc-env=CRGX_TARGET=<triple>` and reads it back
            // with `env!("CRGX_TARGET")`. Cargo sets such vars only on
            // the rustc process env; if the dispatch filter drops them,
            // the daemon-spawned rustc fails with `error: environment
            // variable ... not defined at compile time`. The names are
            // arbitrary, so the filter must be a noise denylist that
            // forwards anything it does not recognize.
            for v in [
                "CRGX_TARGET",
                "VERGEN_GIT_SHA",
                "SHADOW_RS",
                "SOME_CRATE_CUSTOM_COMPILE_TIME_VAR",
            ] {
                assert!(
                    is_compile_env_var(v),
                    "{v} must be forwarded to the daemon — build-script \
                     `cargo:rustc-env` vars have arbitrary names and dropping \
                     them breaks `env!()` in daemon-spawned rustc"
                );
            }
        }
    );

    timed_test!(
        is_compile_env_var_forwards_apple_sdk_vars,
        Duration::from_secs(5),
        {
            // Regression test for the darwin cross-compile lanes (run
            // 28574600982). The workflow exports SDKROOT
            // before `soldr cargo zigbuild --target *-apple-darwin`;
            // rustc reads it to locate the Apple SDK when linking (it
            // appends `-isysroot <sdk>` to the cc-style linker), and the
            // zig-cc linker shim reads it again for the SDK library
            // search path. The daemon replays the filtered env into
            // rustc (env_clear + replay), so dropping any of these
            // makes every `-lobjc` / `-framework` link fail with
            // "unable to find dynamic system library 'objc'".
            for v in ["SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET"] {
                assert!(is_compile_env_var(v), "{v} must be forwarded to daemon");
            }
        }
    );

    timed_test!(
        is_compile_env_var_drops_common_noise,
        Duration::from_secs(5),
        {
            for v in [
                "PROMPT",
                "PSModulePath",
                "ChocolateyInstall",
                "WSL_DISTRO_NAME",
            ] {
                assert!(!is_compile_env_var(v), "{v} should be dropped");
            }
        }
    );

    timed_test!(
        build_compile_request_filters_env_and_carries_args,
        Duration::from_secs(5),
        {
            std::env::set_var("CARGO_PKG_NAME_TEST_DISPATCH", "soldr-cli-test");
            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let req = build_compile_request(&argv);
            std::env::remove_var("CARGO_PKG_NAME_TEST_DISPATCH");

            assert_eq!(req.args, argv);
            assert!(
                req.env
                    .iter()
                    .any(|(k, _)| k == "CARGO_PKG_NAME_TEST_DISPATCH"),
                "CARGO_*-prefixed env var must survive the filter"
            );
        }
    );

    timed_test!(
        resolved_spawn_retry_budget_respects_override,
        Duration::from_secs(5),
        {
            let g = BudgetEnvGuard::acquire();
            g.set("250");
            let budget = resolved_spawn_retry_budget();
            drop(g);
            assert_eq!(budget, Duration::from_millis(250));
        }
    );

    timed_test!(
        resolved_spawn_retry_budget_clamps_to_min_100ms,
        Duration::from_secs(5),
        {
            // Defense in depth: no caller can disable retries entirely
            // by passing 0 — the loop must still get at least one shot.
            let g = BudgetEnvGuard::acquire();
            g.set("0");
            let budget = resolved_spawn_retry_budget();
            drop(g);
            assert!(
                budget >= Duration::from_millis(100),
                "budget {budget:?} must be clamped to at least 100ms"
            );
        }
    );

    // The TDD acceptance test for the no-hang contract: point dispatch
    // at a socket path that cannot possibly accept, set the budget to
    // 250 ms, expect the call to return an Err in well under 2 seconds.
    // The `timed_test!` 10-second deadline is the belt-and-suspenders —
    // if a regression makes the dispatch ignore the budget, the test
    // hangs there and the watchdog fires.
    timed_test!(
        dispatch_compile_with_sock_fails_within_budget_on_dead_socket,
        Duration::from_secs(15),
        {
            let g = BudgetEnvGuard::acquire();
            // 250 ms budget; we expect dispatch to fail-fast.
            g.set("250");

            // A path that cannot resolve to a live socket / pipe on any
            // platform. On Windows the leading `\\.\pipe\` prefix is
            // mandatory for named pipes, so a Unix-style path under TMP
            // cannot accept; on Unix the path simply does not exist.
            let dead = if cfg!(windows) {
                PathBuf::from(r"\\.\pipe\soldr-test-no-such-pipe-12345")
            } else {
                std::env::temp_dir().join("soldr-test-no-such-sock-12345")
            };
            // Make sure no leftover artifact from a prior test is on disk.
            let _ = std::fs::remove_file(&dead);

            let argv = vec!["rustc".to_string(), "--version".to_string()];
            let mut stdout: Vec<u8> = Vec::new();
            let mut stderr: Vec<u8> = Vec::new();

            let start = Instant::now();
            let result = dispatch_compile_with_sock(&dead, &argv, &mut stdout, &mut stderr);
            let elapsed = start.elapsed();
            drop(g);

            assert!(result.is_err(), "dispatch should error on dead socket");
            // Windows runtime spawn overhead per attempt can push elapsed
            // well past the configured budget. The contract is "the loop
            // honors the budget" — i.e. we don't sit at the 30 s default —
            // not a tight stopwatch on absolute time. A 5 s ceiling is
            // generous enough to absorb tokio runtime startup while still
            // catching a regression that ignores the budget entirely.
            assert!(
                elapsed < Duration::from_secs(5),
                "dispatch took {elapsed:?} on a dead socket with a 250 ms budget — \
             this is the no-hang contract: the retry loop MUST honor the \
             SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS budget instead of waiting \
             out the 30s default"
            );
        }
    );
}
