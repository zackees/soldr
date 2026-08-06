//! Environment composition for the detached daemon spawn.
//!
//! All three spawn paths build the child environment the same way:
//! `running-process`'s scrubbed user-baseline environment, with a narrow
//! allowlist overlaid on top. The baseline deliberately is *not* the caller's
//! environment, so a variable reaches the daemon only if the allowlist admits
//! it. See the constants below for why each non-`SOLDR_` exception exists.

/// Env-var name prefix forwarded from the spawning process into the
/// detached daemon on top of running-process's user-baseline environment.
///
/// running-process 4.6.1 rebuilds a scrubbed login environment on Unix
/// (Windows has always done so via `CreateEnvironmentBlock`), which
/// silently dropped `SOLDR_CACHE_DIR`: the daemon bound its socket under
/// the default `~/.soldr` root while wrappers polled
/// `$SOLDR_CACHE_DIR/cache/soldr-daemon/sock`, hit `NotRunning` for the
/// full spawn-retry budget, and every compile fell back to direct
/// uncached rustc (the soldr#1657 degradation path firing on all of CI).
/// All soldr-owned configuration must survive the spawn boundary, so the
/// whole `SOLDR_*` namespace is overlaid onto the baseline. The embedded
/// zccache trace below is the sole non-Soldr diagnostic exception.
pub(crate) const FORWARDED_ENV_PREFIX: &str = "SOLDR_";
/// The `ZCCACHE_*` names that must survive the scrub, and why each one does.
///
/// The rule is not "zccache variables are forwarded" -- `ZCCACHE_DISABLE` is
/// deliberately dropped, and the test below asserts that. The rule is
/// narrower: **a variable crosses when the daemon's own process is what reads
/// it.** Anything consumed by the caller before it ever spawns a daemon has
/// no reason to cross, and forwarding it would only widen the surface.
///
/// - `ZCCACHE_INNER_TRACE` -- opt-in write-only diagnostic trace. The embedded
///   backend runs *inside* soldr-daemon, so the trace the caller asked for is
///   only producible on the far side of the spawn.
/// - `ZCCACHE_MAX_PARALLEL_COMPILES` -- soldr#1931. `core::jobs` resolves the
///   compile limit in the daemon process (`daemon/server.rs`,
///   `zccache_embedded.rs`), reading this name with `std::env::var`. Scrubbed,
///   that resolver tier can never fire on the auto-spawn path -- which is the
///   only path normal use takes -- so a machine tuned before soldr#1902
///   silently reverts to the default. Forwarded under its real name rather
///   than promoted to `SOLDR_JOBS` at the boundary: promotion would let a
///   legacy export outrank `[jobs].max_parallel_compiles`, inverting the
///   documented precedence. Precedence stays resolved in exactly one place.
/// - `ZCCACHE_STAGING_DIR` -- soldr#2188. The embedded service reads this
///   override inside the detached daemon so Windows compilers receive a short
///   private output path even when `SOLDR_CACHE_DIR` is deeply nested.
pub(crate) const FORWARDED_ZCCACHE_ENV: &[&str] = &[
    "ZCCACHE_INNER_TRACE",
    crate::core::jobs::ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR,
    zccache::core::config::STAGING_DIR_ENV,
];

/// The environment overlay applied on top of `running-process`'s user
/// baseline. `running-process` adds its positive daemon declaration itself.
pub(crate) fn daemon_spawn_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    forwarded_soldr_env()
}

pub(crate) fn forwarded_soldr_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    filter_forwarded_env(std::env::vars_os())
}

/// Pure filter behind [`forwarded_soldr_env`], split out so tests can
/// exercise it without mutating the process environment (parallel test
/// cases in this binary read the real env).
pub(crate) fn filter_forwarded_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            // Env names compare case-insensitively on Windows; match the
            // FBUILD_* passthrough in FastLED/fbuild#1170 and accept any
            // casing of the prefix on every platform.
            let name = name.to_string_lossy().to_ascii_uppercase();
            name.starts_with(FORWARDED_ENV_PREFIX) || FORWARDED_ZCCACHE_ENV.contains(&name.as_str())
        })
        .collect()
}
