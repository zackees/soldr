//! Toolchain / rustup / zccache binary resolution helpers. Extracted from
//! `main.rs` as part of issue #339.

use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, SoldrError, SoldrPaths,
};
use crate::fetch::VersionSpec;
use crate::{
    REAL_TOOLCHAIN_BINARY_ENV_PREFIX, TEST_CARGO_BIN_ENV_VAR, TEST_RUSTC_BIN_ENV_VAR,
    TEST_RUSTUP_BIN_ENV_VAR, TEST_ZCCACHE_BIN_ENV_VAR,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Escape hatch: `SOLDR_TOOLCHAIN_BIN_CACHE=off` disables both the
/// in-process memo and the on-disk cache for channel-scoped
/// `rustup which` resolution (see [`resolve_toolchain_binary_for_channel`]).
pub(crate) const TOOLCHAIN_BIN_CACHE_ENV_VAR: &str = "SOLDR_TOOLCHAIN_BIN_CACHE";

pub(crate) fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }

    let start_dir = std::env::current_dir().ok();
    if let Some(path) = probe_direct_toolchain_binary(tool, start_dir.as_deref()) {
        return Ok(path);
    }

    resolve_toolchain_binary_with_optional_channel(tool, None, start_dir.as_deref())
}

/// Resolve `tool` for an explicit, immutable `channel` (e.g.
/// `nightly-2026-01-18`), memoizing the result in-process and on disk
/// so nested cargo-dylint re-entries don't each pay a fresh `rustup
/// which` subprocess spawn. The ambient-default lookup (no channel) is
/// intentionally NOT disk-cached: it is not keyed by anything stable —
/// the default toolchain can change without any cache key changing.
pub(crate) fn resolve_toolchain_binary_for_channel(
    tool: &str,
    channel: Option<&str>,
) -> Result<std::path::PathBuf, SoldrError> {
    let Some(channel) = channel.map(str::trim).filter(|channel| !channel.is_empty()) else {
        return resolve_toolchain_binary(tool);
    };

    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }

    let cache_enabled = !toolchain_bin_cache_disabled();

    if cache_enabled {
        if let Some(path) = toolchain_bin_memo_lookup(channel, tool) {
            return Ok(path);
        }
        if let Some(path) = disk_cache_lookup(channel, tool) {
            toolchain_bin_memo_store(channel, tool, path.clone());
            return Ok(path);
        }
    }

    let resolved = resolve_toolchain_binary_with_optional_channel(tool, Some(channel), None)?;

    if cache_enabled && resolved.is_file() {
        toolchain_bin_memo_store(channel, tool, resolved.clone());
        disk_cache_store(channel, tool, &resolved);
    }

    Ok(resolved)
}

fn toolchain_bin_cache_disabled() -> bool {
    std::env::var_os(TOOLCHAIN_BIN_CACHE_ENV_VAR)
        .map(|value| value.to_string_lossy().trim().eq_ignore_ascii_case("off"))
        .unwrap_or(false)
}

type ToolchainBinMemo = Mutex<HashMap<(String, String), std::path::PathBuf>>;

fn toolchain_bin_memo() -> &'static ToolchainBinMemo {
    static MEMO: OnceLock<ToolchainBinMemo> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn toolchain_bin_memo_lookup(channel: &str, tool: &str) -> Option<std::path::PathBuf> {
    let memo = toolchain_bin_memo().lock().ok()?;
    memo.get(&(channel.to_string(), tool.to_string())).cloned()
}

fn toolchain_bin_memo_store(channel: &str, tool: &str, path: std::path::PathBuf) {
    if let Ok(mut memo) = toolchain_bin_memo().lock() {
        memo.insert((channel.to_string(), tool.to_string()), path);
    }
}

/// Sanitize a toolchain channel string for use as a single path
/// component. Rustup channel names are already path-safe in practice
/// (`nightly-2026-01-18`, `1.94.1-x86_64-pc-windows-msvc`) but this
/// defends against any unexpected separator/traversal characters.
fn sanitize_toolchain_for_path(channel: &str) -> String {
    channel
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// `<cache_root>/toolchain-bins/v1/<sanitized-toolchain>/<tool>.path`
/// Split out with an explicit `cache_root` parameter so unit tests can
/// point it at a tempdir instead of the real `~/.soldr/cache`.
fn toolchain_bin_disk_cache_path_in(
    cache_root: &std::path::Path,
    channel: &str,
    tool: &str,
) -> std::path::PathBuf {
    cache_root
        .join("toolchain-bins")
        .join("v1")
        .join(sanitize_toolchain_for_path(channel))
        .join(format!("{tool}.path"))
}

fn disk_cache_lookup_in(
    cache_root: &std::path::Path,
    channel: &str,
    tool: &str,
) -> Option<std::path::PathBuf> {
    let cache_file = toolchain_bin_disk_cache_path_in(cache_root, channel, tool);
    let contents = std::fs::read_to_string(&cache_file).ok()?;
    let candidate = std::path::PathBuf::from(contents.trim());
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

fn disk_cache_store_in(
    cache_root: &std::path::Path,
    channel: &str,
    tool: &str,
    resolved: &std::path::Path,
) {
    let cache_file = toolchain_bin_disk_cache_path_in(cache_root, channel, tool);
    let Some(parent) = cache_file.parent() else {
        return;
    };
    // Best-effort: a write failure here must never fail resolution —
    // the caller already has a good `resolved` path in hand.
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = std::fs::write(&cache_file, format!("{}\n", resolved.display()));
}

fn disk_cache_lookup(channel: &str, tool: &str) -> Option<std::path::PathBuf> {
    let paths = SoldrPaths::new().ok()?;
    disk_cache_lookup_in(&paths.cache, channel, tool)
}

fn disk_cache_store(channel: &str, tool: &str, resolved: &std::path::Path) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    disk_cache_store_in(&paths.cache, channel, tool, resolved);
}

fn resolve_toolchain_binary_with_optional_channel(
    tool: &str,
    channel: Option<&str>,
    fallback_start_dir: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.arg("which");
    if let Some(channel) = channel {
        command.args(["--toolchain", channel]);
    }
    command.arg(tool);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let context = match channel {
        Some(channel) => format!("rustup which --toolchain {channel} {tool}"),
        None => format!("rustup which {tool}"),
    };
    let output = command_output_with_timeout(&mut command, &context);

    match output {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path.into());
            }
        }
        Ok(output) => {
            if channel.is_none() {
                if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
                    return Ok(path);
                }
            }
            return Err(rustup_resolution_failure(tool, &output.stderr));
        }
        Err(err) => {
            if channel.is_none() {
                if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
                    return Ok(path);
                }
            }
            return Err(SoldrError::Other(format!(
                "failed to invoke rustup while resolving {tool}: {err}"
            )));
        }
    }

    if channel.is_none() {
        if let Some(path) = crate::core::probe_toolchain_binary(tool, fallback_start_dir) {
            return Ok(path);
        }
    }

    Err(SoldrError::Other(format!(
        "rustup did not return a path for {tool}"
    )))
}

fn probe_direct_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some_and(|value| !value.is_empty()) {
        return None;
    }

    explicit_rustup_toolchain_binary(tool)
        .or_else(|| repo_local_rustup_toolchain_binary(tool, start_dir))
        .or_else(|| explicit_cargo_home_binary(tool))
        .or_else(|| repo_local_cargo_home_binary(tool, start_dir))
}

fn explicit_cargo_home_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("CARGO_HOME").and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn repo_local_cargo_home_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".cargo")
        .and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn explicit_rustup_toolchain_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("RUSTUP_HOME")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn repo_local_rustup_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".rustup")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn rustup_home_single_toolchain_binary(
    rustup_home: &std::path::Path,
    tool: &str,
) -> Option<std::path::PathBuf> {
    let mut candidates = std::fs::read_dir(rustup_home.join("toolchains"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter_map(|dir| executable_in_dir(&dir, tool))
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn find_ancestor_dir(
    start_dir: Option<&std::path::Path>,
    relative: &str,
) -> Option<std::path::PathBuf> {
    let user_home = crate::core::user_home_dir().ok();
    find_ancestor_dir_bounded(start_dir?, relative, user_home.as_deref())
}

fn find_ancestor_dir_bounded(
    start_dir: &std::path::Path,
    relative: &str,
    user_home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    // Normalize both sides before comparing. This is load-bearing on Windows,
    // where temp paths may use an 8.3 alias (RUNNER~1) while USERPROFILE uses
    // the long spelling; lexical equality would walk straight through the
    // intended boundary into the runner's global toolchain homes.
    let mut current = std::fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());
    let user_home =
        user_home.map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    loop {
        // A project may live below the user's home, but the home directory's
        // own `.cargo` / `.rustup` is global state, not a repository-local
        // toolchain. Stop before inspecting it so an explicit rustup resolver
        // (including the test seam) can choose the intended tool instead.
        if user_home.as_deref() == Some(current.as_path()) {
            return None;
        }
        let candidate = current.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn executable_in_dir(dir: &std::path::Path, tool: &str) -> Option<std::path::PathBuf> {
    let candidate = dir.join(tool);
    if candidate.is_file() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        for suffix in windows_path_exts() {
            let candidate = dir.join(format!("{tool}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_path_exts() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

pub(crate) fn apply_implicit_toolchain_homes(command: &mut std::process::Command) {
    let start_dir = std::env::current_dir().ok();
    crate::core::apply_implicit_toolchain_homes(command, start_dir.as_deref());
    apply_managed_toolchain_homes_if_available(command, start_dir.as_deref());
}

/// Apply homes that match a toolchain binary Soldr already resolved.
///
/// Rustup discovery and bootstrap intentionally use Soldr's managed homes,
/// but a concrete host binary must keep the caller's host Rustup context.
/// Mixing a host Cargo/rustfmt proxy with Soldr's default-less managed
/// `RUSTUP_HOME` makes Rustup report that no default toolchain is configured.
pub(crate) fn apply_resolved_toolchain_homes(
    command: &mut std::process::Command,
    binary: &std::path::Path,
) {
    let start_dir = std::env::current_dir().ok();
    crate::core::apply_implicit_toolchain_homes(command, start_dir.as_deref());

    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    let managed_cargo_home = crate::fetch::managed_cargo_home(&paths);
    let managed_rustup_home = crate::fetch::managed_rustup_home(&paths);
    if path_is_within(binary, &managed_cargo_home) || path_is_within(binary, &managed_rustup_home) {
        apply_managed_toolchain_homes_if_available(command, start_dir.as_deref());
    }
}

fn path_is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

fn apply_managed_toolchain_homes_if_available(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
) {
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    apply_managed_cargo_home_if_available_for_paths(command, start_dir, &paths);
    apply_managed_rustup_home_if_available_for_paths(command, start_dir, &paths);
}

/// Apply only Soldr's managed Cargo home when it is implicit and available.
///
/// Tool-acquisition paths use this after [`apply_resolved_toolchain_homes`]:
/// plugins still install into Soldr's managed Cargo root, while a host-owned
/// Cargo binary keeps its host Rustup context.
pub(crate) fn apply_managed_cargo_home_if_available(command: &mut std::process::Command) {
    let start_dir = std::env::current_dir().ok();
    let Ok(paths) = SoldrPaths::new() else {
        return;
    };
    apply_managed_cargo_home_if_available_for_paths(command, start_dir.as_deref(), &paths);
}

fn apply_managed_cargo_home_if_available_for_paths(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
    paths: &SoldrPaths,
) {
    if std::env::var_os(crate::core::CARGO_HOME_ENV_VAR).is_none()
        && find_ancestor_dir(start_dir, ".cargo").is_none()
    {
        let managed = crate::fetch::managed_cargo_home(paths);
        if managed.is_dir() {
            command.env(crate::core::CARGO_HOME_ENV_VAR, managed);
        }
    }
}

fn apply_managed_rustup_home_if_available_for_paths(
    command: &mut std::process::Command,
    start_dir: Option<&std::path::Path>,
    paths: &SoldrPaths,
) {
    if std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR).is_none()
        && find_ancestor_dir(start_dir, ".rustup").is_none()
    {
        let managed = crate::fetch::managed_rustup_home(paths);
        if managed.is_dir() {
            command.env(crate::core::RUSTUP_HOME_ENV_VAR, managed);
        }
    }
}

pub(crate) fn rustup_resolution_failure(tool: &str, stderr: &[u8]) -> SoldrError {
    let raw_failure = String::from_utf8_lossy(stderr).trim().to_string();
    SoldrError::Other(format!(
        "failed to resolve {tool} via rustup: {raw_failure}\n\
CI hint: if this repository pins Rust in rust-toolchain.toml, preinstall that exact channel instead of a generic stable toolchain.\n\
CI hint: export RUSTUP_TOOLCHAIN to that exact channel for later cargo, rustc, and soldr cargo steps, or use the documented setup-soldr action path (uses: zackees/soldr@<ref> or uses: ./).\n\
Bootstrap hint: if rustup itself is missing on this host, run `soldr bootstrap` to install it into soldr's managed bin dir (or unset SOLDR_NO_BOOTSTRAP and re-run if you previously opted out)."
    ))
}

pub(crate) fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
    }
}

fn toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    let env_var = match tool {
        "cargo" => TEST_CARGO_BIN_ENV_VAR,
        "rustc" => TEST_RUSTC_BIN_ENV_VAR,
        _ => return real_toolchain_binary_override(tool),
    };
    non_empty_env_path(env_var).or_else(|| real_toolchain_binary_override(tool))
}

fn real_toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path(&real_toolchain_binary_env_var(tool))
}

fn real_toolchain_binary_env_var(tool: &str) -> String {
    let mut value = String::from(REAL_TOOLCHAIN_BINARY_ENV_PREFIX);
    for ch in tool.chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch.to_ascii_uppercase());
        } else {
            value.push('_');
        }
    }
    value
}

/// Resolve the rustup binary to spawn for any subprocess call. Always
/// honours `SOLDR_TEST_RUSTUP_BIN` for tests. Then prefers `rustup` already on
/// `PATH` or the soldr-managed copy under `<SoldrPaths::bin>/rustup`. If
/// neither exists and the user has not opted out via `SOLDR_NO_BOOTSTRAP=1`,
/// triggers a one-shot bootstrap (`rustup-init`) that installs rustup into
/// the soldr-managed bin dir before returning that path.
pub(crate) fn rustup_binary() -> std::path::PathBuf {
    if let Some(path) = non_empty_env_path(TEST_RUSTUP_BIN_ENV_VAR) {
        return path;
    }
    if let Ok(paths) = SoldrPaths::new() {
        if let Some(existing) = crate::fetch::discover_rustup(&paths) {
            return existing;
        }
        match crate::fetch::auto_bootstrap_if_missing_blocking(&paths) {
            Ok(crate::fetch::AutoBootstrapOutcome::AlreadyInstalled(p)) => return p,
            Ok(crate::fetch::AutoBootstrapOutcome::Installed(report)) => return report.rustup_path,
            Ok(crate::fetch::AutoBootstrapOutcome::OptedOut) => {
                // Fall through to the bare "rustup" path; the subsequent
                // resolution failure surfaces the standard diagnostic plus
                // the `SOLDR_NO_BOOTSTRAP` hint.
            }
            Err(err) => {
                eprintln!(
                    "soldr: auto-bootstrap failed ({err}). Falling back to `rustup` on PATH. \
                     Run `soldr bootstrap` manually or unset {} to retry.",
                    crate::fetch::NO_BOOTSTRAP_ENV_VAR
                );
            }
        }
    }
    "rustup".into()
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<std::path::PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(value.into())
}

pub(crate) fn current_soldr_binary() -> Result<std::path::PathBuf, SoldrError> {
    std::env::current_exe().map_err(SoldrError::from)
}

/// Materialize a compiler-named multicall shim for use as Cargo's managed
/// wrapper. The current executable may itself be a Cargo multicall hardlink,
/// so it is not a safe wrapper path: Cargo would invoke that alias with the
/// compiler path and re-enter the front door. A versioned compiler identity
/// makes the wrapper contract explicit and keeps soldr versions isolated.
pub(crate) fn rustc_wrapper_shim_binary(
    paths: &SoldrPaths,
) -> Result<std::path::PathBuf, SoldrError> {
    let target = paths
        .versioned_shims_dir()
        .join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    let source = crate::shim_materialize::soldr_binary_source()?;
    crate::shim_materialize::materialize_executable(&source, &target)?;
    Ok(target)
}

/// Materialize the daemon's stable process/service identity next to soldr.
pub(crate) fn soldr_daemon_binary() -> Result<std::path::PathBuf, SoldrError> {
    materialize_runtime_alias("soldr-daemon")
}

/// Ensure compiler-side daemon recovery has a canonically named executable.
///
/// The managed Cargo front door normally injects this handoff once for every
/// compiler child. Direct `RUSTC_WRAPPER` / `zccache-soldr` invocations do not
/// have that parent, so recover it lazily after the first failed daemon probe.
/// Reuse an existing sibling without hashing; only first use materializes the
/// multicall alias.
pub(crate) fn ensure_daemon_executable_handoff() -> Result<std::path::PathBuf, SoldrError> {
    let env_var = crate::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR;
    if let Some(configured) = non_empty_env_path(env_var).filter(|path| {
        path.is_file()
            && path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|stem| stem.eq_ignore_ascii_case("soldr-daemon"))
    }) {
        return Ok(configured);
    }

    let current = std::env::current_exe().map_err(SoldrError::from)?;
    let sibling = current.parent().map(|parent| {
        parent.join(if cfg!(windows) {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        })
    });
    let daemon = sibling
        .filter(|path| path.is_file())
        .map(Ok)
        .unwrap_or_else(soldr_daemon_binary)?;
    std::env::set_var(env_var, &daemon);
    Ok(daemon)
}

fn materialize_runtime_alias(stem: &str) -> Result<std::path::PathBuf, SoldrError> {
    let source = crate::shim_materialize::soldr_binary_source()?;
    let current = std::env::current_exe().map_err(SoldrError::from)?;
    let file = if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    };
    let (current_target, source_target) = runtime_alias_targets(&source, &current, &file)?;
    crate::shim_materialize::materialize_executable(&source, &current_target)?;

    // Self-relocated wrapper invocations run from a GC-managed runtime dir,
    // while `soldr_binary_source()` intentionally points at the package-owned
    // original. Keep both layouts complete: lifecycle resolves beside
    // `current_exe`, and package commands resolve beside the original.
    if let Some(source_target) = source_target {
        crate::shim_materialize::materialize_executable(&source, &source_target)?;
    }
    Ok(current_target)
}

fn runtime_alias_targets(
    source: &std::path::Path,
    current: &std::path::Path,
    file: &str,
) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), SoldrError> {
    let current_parent = current.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "running soldr has no parent: {}",
            current.display()
        ))
    })?;
    let current_target = current_parent.join(file);
    let source_target = source
        .parent()
        .map(|parent| parent.join(file))
        .filter(|target| *target != current_target);
    Ok((current_target, source_target))
}

/// Materialize the `zccache-soldr` RUSTC_WRAPPER/CC shim name
/// (soldr#1081) from the main `soldr` binary. Native-C caching
/// injects this stable basename into `CC`/`CXX` so cc-rs build-script
/// compiles route through the soldr-daemon embedded zccache service
/// over the `Request::Compile` IPC verb.
pub(crate) fn zccache_soldr_shim_binary() -> Result<std::path::PathBuf, SoldrError> {
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let file = if cfg!(windows) {
        "zccache-soldr.exe"
    } else {
        "zccache-soldr"
    };
    let target = paths.bin.join(file);
    let source = crate::shim_materialize::soldr_binary_source()?;
    crate::shim_materialize::materialize_executable(&source, &target)?;
    Ok(target)
}

/// Resolve `<stem>` as a sibling of the running executable (adding
/// `.exe` on Windows), falling back to the bare stem when the sibling
/// is absent so a PATH lookup can still find it.
fn sibling_binary(stem: &str) -> std::path::PathBuf {
    let file = if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(&file);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(managed_wrapper_shim_has_compiler_identity, {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(root.path().join("soldr"));

        let wrapper = rustc_wrapper_shim_binary(&paths).expect("materialize wrapper shim");

        assert!(wrapper.is_file(), "missing {}", wrapper.display());
        assert_eq!(
            wrapper.file_stem().and_then(std::ffi::OsStr::to_str),
            Some("rustc")
        );
        assert_eq!(
            wrapper.parent(),
            Some(paths.versioned_shims_dir().as_path())
        );
    });

    crate::timed_test!(
        relocated_runtime_alias_is_materialized_beside_current_exe,
        {
            let source = std::path::Path::new("/opt/package/bin/soldr");
            let current = std::path::Path::new("/tmp/runtime/hash/soldr");
            let (current_target, source_target) =
                runtime_alias_targets(source, current, "soldr-daemon").expect("alias targets");
            assert_eq!(
                current_target,
                std::path::Path::new("/tmp/runtime/hash/soldr-daemon")
            );
            assert_eq!(
                source_target.as_deref(),
                Some(std::path::Path::new("/opt/package/bin/soldr-daemon"))
            );
        }
    );

    crate::timed_test!(ancestor_search_canonicalizes_the_user_home_boundary, {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let nested = home.join("nested");
        std::fs::create_dir_all(home.join(".cargo")).expect("create global toolchain directory");
        std::fs::create_dir_all(&nested).expect("create nested directory");

        let alternate_home_spelling = nested.join("..");
        assert!(
            find_ancestor_dir_bounded(&alternate_home_spelling, ".cargo", Some(&home)).is_none(),
            "the user home's global toolchain directory is not repository-local"
        );

        let project = nested.join("project");
        let project_tools = project.join(".cargo");
        std::fs::create_dir_all(&project_tools).expect("create project-local toolchain directory");
        let canonical_project_tools =
            std::fs::canonicalize(&project_tools).expect("canonicalize project tool directory");
        assert_eq!(
            find_ancestor_dir_bounded(&project, ".cargo", Some(&home)).as_deref(),
            Some(canonical_project_tools.as_path()),
        );
    });

    #[test]
    fn parse_tool_spec_defaults_to_latest_version() {
        let (tool, version) = parse_tool_spec("maturin");
        assert_eq!(tool, "maturin");
        assert!(matches!(version, VersionSpec::Latest));
    }

    #[test]
    fn rustup_resolution_failure_appends_ci_guidance() {
        let error = rustup_resolution_failure(
            "rustc",
            b"error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed",
        );

        let rendered = error.to_string();
        assert!(rendered.contains("failed to resolve rustc via rustup: error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed"));
        assert!(rendered.contains("pins Rust in rust-toolchain.toml"));
        assert!(rendered.contains("generic stable toolchain"));
        assert!(rendered.contains("RUSTUP_TOOLCHAIN"));
        assert!(rendered.contains("setup-soldr action path"));
        assert!(rendered.contains("soldr bootstrap"));
        assert!(rendered.contains("SOLDR_NO_BOOTSTRAP"));
    }

    #[test]
    fn known_subcommand_registry_recognizes_phase_two_tools() {
        for sub in ["nextest", "deny", "audit", "llvm-cov"] {
            let spec = crate::fetch::lookup_by_cargo_subcommand(sub)
                .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
            assert_eq!(spec.cargo_subcommand, Some(sub));
            assert!(spec.crate_name.starts_with("cargo-"));
        }
    }

    #[test]
    fn known_subcommand_registry_recognizes_phase_three_tools() {
        for sub in ["udeps", "semver-checks", "expand", "watch"] {
            let spec = crate::fetch::lookup_by_cargo_subcommand(sub)
                .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
            assert_eq!(spec.cargo_subcommand, Some(sub));
            assert!(spec.crate_name.starts_with("cargo-"));
        }
    }

    #[test]
    fn top_level_tools_are_not_cargo_subcommands() {
        for crate_name in [
            "cross",
            "mdbook",
            "cbindgen",
            "wasm-pack",
            "trunk",
            "sccache",
        ] {
            let spec = crate::fetch::lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
        }
    }

    #[test]
    fn soldr_itself_is_registered_for_self_trampoline() {
        let spec = crate::fetch::lookup_by_crate("soldr")
            .expect("soldr should be registered in known_tools for --as trampoline");
        assert_eq!(spec.binary_name, "soldr");
        assert_eq!(spec.repo, Some(("zackees", "soldr")));
        assert_eq!(spec.cargo_subcommand, None);
    }

    // -----------------------------------------------------------------
    // Channel-scoped rustup-which disk cache (nested dylint re-entry
    // overhead reduction). These use the `_in` variants with an
    // injected `cache_root` tempdir so they never touch the real
    // `~/.soldr/cache`.
    // -----------------------------------------------------------------

    #[test]
    fn sanitize_toolchain_for_path_replaces_unsafe_characters() {
        assert_eq!(
            sanitize_toolchain_for_path("nightly-2026-01-18"),
            "nightly-2026-01-18"
        );
        assert_eq!(
            sanitize_toolchain_for_path("weird/../channel:name"),
            "weird___channel_name"
        );
    }

    crate::timed_test!(disk_cache_lookup_returns_none_when_uncached, {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(disk_cache_lookup_in(root.path(), "nightly-2026-01-18", "rustc").is_none());
    });

    crate::timed_test!(disk_cache_round_trips_a_resolved_path, {
        let root = tempfile::tempdir().expect("tempdir");
        // The disk cache only trusts entries whose target file still
        // exists, so materialize a real file to point at.
        let resolved = root.path().join("rustc-real");
        std::fs::write(&resolved, b"stub").expect("write stub binary");

        disk_cache_store_in(root.path(), "nightly-2026-01-18", "rustc", &resolved);
        let looked_up = disk_cache_lookup_in(root.path(), "nightly-2026-01-18", "rustc")
            .expect("cache hit after store");
        assert_eq!(looked_up, resolved);

        let cache_file =
            toolchain_bin_disk_cache_path_in(root.path(), "nightly-2026-01-18", "rustc");
        assert!(cache_file.is_file());
        assert!(cache_file.starts_with(root.path().join("toolchain-bins").join("v1")));
    });

    crate::timed_test!(disk_cache_ignores_stale_entry_whose_target_is_gone, {
        let root = tempfile::tempdir().expect("tempdir");
        let resolved = root.path().join("rustc-real");
        std::fs::write(&resolved, b"stub").expect("write stub binary");
        disk_cache_store_in(root.path(), "nightly-2026-01-18", "rustc", &resolved);

        std::fs::remove_file(&resolved).expect("remove target to simulate staleness");

        assert!(
            disk_cache_lookup_in(root.path(), "nightly-2026-01-18", "rustc").is_none(),
            "a cache entry pointing at a missing file must not be trusted"
        );
    });

    #[test]
    fn toolchain_bin_memo_round_trips() {
        let path = std::path::PathBuf::from("/does/not/matter/rustc");
        assert!(toolchain_bin_memo_lookup("memo-test-channel", "rustc").is_none());
        toolchain_bin_memo_store("memo-test-channel", "rustc", path.clone());
        assert_eq!(
            toolchain_bin_memo_lookup("memo-test-channel", "rustc"),
            Some(path)
        );
    }

    #[test]
    fn toolchain_bin_cache_disabled_only_on_off_value() {
        // Test seam: mutate the process-global env var, observe the
        // gate, then restore whatever was there before so this test
        // does not leak state into others in the binary.
        let previous = std::env::var_os(TOOLCHAIN_BIN_CACHE_ENV_VAR);

        std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "off");
        assert!(toolchain_bin_cache_disabled());

        std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "OFF");
        assert!(
            toolchain_bin_cache_disabled(),
            "gate must be case-insensitive"
        );

        std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, "on");
        assert!(!toolchain_bin_cache_disabled());

        std::env::remove_var(TOOLCHAIN_BIN_CACHE_ENV_VAR);
        assert!(
            !toolchain_bin_cache_disabled(),
            "unset must default to enabled"
        );

        match previous {
            Some(value) => std::env::set_var(TOOLCHAIN_BIN_CACHE_ENV_VAR, value),
            None => std::env::remove_var(TOOLCHAIN_BIN_CACHE_ENV_VAR),
        }
    }
}
