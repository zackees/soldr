//! Environment composition for the detached daemon spawn.
//!
//! All three spawn paths build the child environment the same way:
//! `running-process`'s scrubbed user-baseline environment, with
//! [`daemon_spawn_env_overlay`] on top. The baseline deliberately is *not* the
//! caller's environment, so a variable reaches the daemon only if the overlay
//! puts it there.
//!
//! The overlay has two halves: variables forwarded *from* the caller (the
//! `SOLDR_*` namespace plus a short allowlist of `ZCCACHE_*` names, each
//! justified at its constant below), and the daemon marker asserted *by*
//! soldr, which has no source in the parent environment at all.

use std::path::Path;

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
pub(crate) const FORWARDED_ZCCACHE_ENV: &[&str] = &[
    "ZCCACHE_INNER_TRACE",
    crate::core::jobs::ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR,
];

/// Everything overlaid onto the baseline for a daemon spawn: the forwarded
/// caller variables, plus soldr's declaration that the child is a daemon.
///
/// The two are separate concerns deliberately. [`forwarded_soldr_env`] carries
/// values *from* the caller; the daemon marker is asserted *by* soldr and has
/// no source in the parent environment. Composing them here rather than at each
/// spawn site is what keeps the three paths -- unix, unix via-self, and the
/// Windows `CreateProcessW` block -- from drifting apart, which is the failure
/// mode soldr#1931 was.
pub(crate) fn daemon_spawn_env_overlay() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut overlay = forwarded_soldr_env();
    overlay.push((
        std::ffi::OsString::from(running_process::DAEMON_MARKER_ENV_VAR),
        std::ffi::OsString::from("1"),
    ));
    overlay
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

/// Windows counterpart of the Unix `envs(daemon_spawn_env_overlay())` overlay:
/// take running-process's user-baseline pairs, overlay the current
/// process's `SOLDR_*` variables (env names compare case-insensitively on
/// Windows), and serialize to the sorted, double-NUL-terminated UTF-16
/// block `CreateProcessW` expects with `CREATE_UNICODE_ENVIRONMENT`.
#[cfg(windows)]
pub(crate) fn merged_windows_environment_block() -> Result<Vec<u16>, std::io::Error> {
    let pairs = running_process::environment::user_baseline_environment()?;
    Ok(build_windows_environment_block(merge_env_overlay(
        pairs,
        daemon_spawn_env_overlay(),
    )))
}

#[cfg(windows)]
pub(crate) fn merge_env_overlay(
    mut base: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    overlay: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    fn key_upper(name: &std::ffi::OsStr) -> String {
        name.to_string_lossy().to_uppercase()
    }
    for (name, value) in overlay {
        match base
            .iter_mut()
            .find(|(existing, _)| key_upper(existing) == key_upper(&name))
        {
            Some(slot) => slot.1 = value,
            None => base.push((name, value)),
        }
    }
    base.sort_by_key(|(name, _)| key_upper(name));
    base
}

#[cfg(windows)]
pub(crate) fn build_windows_environment_block(
    pairs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut block = Vec::new();
    for (name, value) in pairs {
        block.extend(name.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    // An empty environment block is still two NULs: one for the (absent)
    // final entry, one terminating the block.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}
