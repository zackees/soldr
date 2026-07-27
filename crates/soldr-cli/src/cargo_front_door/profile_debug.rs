//! Cargo `[profile.<X>].debug` default detection and injection.
//!
//! When the user invokes a debug-default cargo subcommand
//! (`build`/`check`/`test`/etc.) without an explicit profile pin in
//! either CLI flags, `--config` overrides, the manifest, or a discovered
//! `.cargo/config.toml`, soldr injects `CARGO_PROFILE_<P>_DEBUG=false`
//! and emits a one-shot warning (deduplicated per repo by the StateDb).
//!
//! All of these helpers are argv-and-filesystem only — they do not
//! touch the running process beyond environment-variable reads and TOML
//! parses, which keeps the cargo front door cheap when the early-out
//! `cargo_profile_debug_default_for_args` returns `None`.

use crate::core::{SoldrError, SoldrPaths};
use crate::{CARGO_PROFILE_DEV_DEBUG_ENV_VAR, CARGO_PROFILE_TEST_DEBUG_ENV_VAR};
use std::collections::BTreeSet;

use super::subcommand::first_cargo_subcommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CargoProfileDebugDefault {
    pub(crate) profile: &'static str,
    pub(crate) env_var: &'static str,
}

impl CargoProfileDebugDefault {
    fn for_profile(profile: &str) -> Option<Self> {
        match profile {
            "dev" | "debug" => Some(Self {
                profile: "dev",
                env_var: CARGO_PROFILE_DEV_DEBUG_ENV_VAR,
            }),
            "test" => Some(Self {
                profile: "test",
                env_var: CARGO_PROFILE_TEST_DEBUG_ENV_VAR,
            }),
            _ => None,
        }
    }

    fn lookup_profiles(self) -> &'static [&'static str] {
        match self.profile {
            "test" => &["test", "dev"],
            _ => &["dev"],
        }
    }
}

pub(super) fn maybe_apply_cargo_profile_debug_default(
    command: &mut std::process::Command,
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Option<CargoProfileDebugDefault>, SoldrError> {
    let Some(default) = cargo_profile_debug_default_for_args(args) else {
        return Ok(None);
    };
    if cargo_profile_debug_is_specified(args, default)? {
        return Ok(None);
    }

    command.env(default.env_var, "false");
    let repo_path = cargo_debug_warning_repo_path(args);
    if should_emit_cargo_debug_default_warning(paths, &repo_path) {
        eprintln!(
            "soldr: warning: Cargo profile.{}.debug is unspecified for {}; setting {}=false for this invocation. Add `debug = true` or `debug = false` under `[profile.{}]` in Cargo.toml or .cargo/config.toml to make this explicit.",
            default.profile,
            repo_path.display(),
            default.env_var,
            default.profile
        );
    }

    Ok(Some(default))
}

fn cargo_profile_debug_default_for_args(args: &[String]) -> Option<CargoProfileDebugDefault> {
    let subcommand = first_cargo_subcommand(args)?;

    if subcommand == "nextest" {
        return if cargo_args_contain_release(args) {
            None
        } else {
            CargoProfileDebugDefault::for_profile("test")
        };
    }

    if cargo_args_contain_release(args) {
        return None;
    }

    if let Some(profile) = cargo_profile_arg_value(args) {
        return CargoProfileDebugDefault::for_profile(&profile);
    }

    match subcommand {
        "t" | "test" => CargoProfileDebugDefault::for_profile("test"),
        "install" if cargo_install_args_contain_debug(args) => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        "install" | "bench" => None,
        "b" | "build" | "c" | "check" | "d" | "doc" | "r" | "run" | "rustc" | "clippy" | "fix" => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        _ => None,
    }
}

fn cargo_profile_debug_is_specified(
    args: &[String],
    default: CargoProfileDebugDefault,
) -> Result<bool, SoldrError> {
    let profiles = default.lookup_profiles();
    if profiles.iter().any(|profile| {
        cargo_profile_debug_env_var(profile)
            .is_some_and(|env_var| std::env::var_os(env_var).is_some())
    }) {
        return Ok(true);
    }

    if cargo_config_args_specify_profile_debug(args, profiles)? {
        return Ok(true);
    }

    let start_dir = cargo_profile_lookup_start_dir(args)?;
    if cargo_manifest_specifies_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }
    if cargo_config_files_specify_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }

    Ok(false)
}

fn cargo_profile_debug_env_var(profile: &str) -> Option<&'static str> {
    match profile {
        "dev" => Some(CARGO_PROFILE_DEV_DEBUG_ENV_VAR),
        "test" => Some(CARGO_PROFILE_TEST_DEBUG_ENV_VAR),
        _ => None,
    }
}

fn cargo_args_contain_release(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--release")
}

fn cargo_profile_arg_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--profile" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_string());
        }
    }
    None
}

fn cargo_install_args_contain_debug(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--debug")
}

fn cargo_config_args_specify_profile_debug(
    args: &[String],
    profiles: &[&str],
) -> Result<bool, SoldrError> {
    let cwd = std::env::current_dir()?;
    for value in cargo_config_arg_values(args) {
        if cargo_config_arg_specifies_profile_debug(&value, &cwd, profiles) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cargo_config_arg_values(args: &[String]) -> Vec<String> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            if let Some(value) = iter.next() {
                values.push(value.clone());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            values.push(value.to_string());
        }
    }
    values
}

fn cargo_config_arg_specifies_profile_debug(
    value: &str,
    cwd: &std::path::Path,
    profiles: &[&str],
) -> bool {
    let raw = value.trim();
    if raw.is_empty() {
        return false;
    }

    let path = std::path::Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.is_file() {
        return toml_file_specifies_profile_debug(&path, profiles);
    }

    toml_text_specifies_profile_debug(raw, profiles)
        .unwrap_or_else(|| raw_may_specify_profile_debug(raw, profiles))
}

fn raw_may_specify_profile_debug(raw: &str, profiles: &[&str]) -> bool {
    let lowered = raw.to_ascii_lowercase();
    profiles.iter().any(|profile| {
        lowered.contains(&format!("profile.{profile}.debug"))
            || (lowered.contains(&format!("[profile.{profile}]")) && lowered.contains("debug"))
    })
}

fn cargo_manifest_specifies_profile_debug(start_dir: &std::path::Path, profiles: &[&str]) -> bool {
    find_workspace_manifest_path(start_dir)
        .is_some_and(|manifest| toml_file_specifies_profile_debug(&manifest, profiles))
}

fn cargo_config_files_specify_profile_debug(
    start_dir: &std::path::Path,
    profiles: &[&str],
) -> bool {
    cargo_config_paths(start_dir)
        .iter()
        .any(|path| toml_file_specifies_profile_debug(path, profiles))
}

fn cargo_config_paths(start_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = BTreeSet::new();
    let mut current = Some(start_dir.to_path_buf());
    while let Some(dir) = current {
        for relative in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(relative);
            if path.is_file() {
                paths.insert(path);
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }

    if let Some(cargo_home) = cargo_home_dir_for_config() {
        for name in ["config.toml", "config"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                paths.insert(path);
            }
        }
    }

    paths.into_iter().collect()
}

fn cargo_home_dir_for_config() -> Option<std::path::PathBuf> {
    std::env::var_os(crate::core::CARGO_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            crate::core::user_home_dir()
                .ok()
                .map(|home| home.join(".cargo"))
        })
}

fn toml_file_specifies_profile_debug(path: &std::path::Path, profiles: &[&str]) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => toml_text_specifies_profile_debug(&text, profiles).unwrap_or(true),
        Err(_) => true,
    }
}

fn toml_text_specifies_profile_debug(text: &str, profiles: &[&str]) -> Option<bool> {
    let value: toml::Value = text.parse().ok()?;
    let Some(profile_table) = value.get("profile") else {
        return Some(false);
    };
    Some(profiles.iter().any(|profile| {
        profile_table
            .get(*profile)
            .and_then(|section| section.get("debug"))
            .is_some()
    }))
}

fn cargo_profile_lookup_start_dir(args: &[String]) -> Result<std::path::PathBuf, SoldrError> {
    let cwd = std::env::current_dir()?;
    let Some(manifest_path) = cargo_manifest_path_arg(args) else {
        return Ok(cwd);
    };
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path
    } else {
        cwd.join(manifest_path)
    };
    let parent = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or(cwd);
    Ok(parent)
}

fn cargo_manifest_path_arg(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--manifest-path" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(std::path::PathBuf::from(value));
        }
    }
    None
}

fn find_workspace_manifest_path(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    let mut nearest_manifest = None;
    let mut workspace_manifest = None;

    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if nearest_manifest.is_none() {
                nearest_manifest = Some(candidate.clone());
            }
            if cargo_manifest_declares_workspace(&candidate) {
                workspace_manifest = Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    workspace_manifest.or(nearest_manifest)
}

fn cargo_manifest_declares_workspace(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return false;
    };
    value.get("workspace").is_some()
}

fn cargo_debug_warning_repo_path(args: &[String]) -> std::path::PathBuf {
    let start_dir = cargo_profile_lookup_start_dir(args)
        .or_else(|_| std::env::current_dir().map_err(SoldrError::from))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    find_git_root(&start_dir)
        .or_else(|| {
            find_workspace_manifest_path(&start_dir)
                .and_then(|manifest| manifest.parent().map(std::path::Path::to_path_buf))
        })
        .unwrap_or(start_dir)
}

fn find_git_root(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn should_emit_cargo_debug_default_warning(
    paths: &SoldrPaths,
    repo_path: &std::path::Path,
) -> bool {
    // soldr#1814 slice 2c: ask the daemon, which owns state_db's tables,
    // rather than making every front-door invocation another opener of
    // state.redb. Falls back to the local read-modify-write only when the
    // daemon is unreachable, so a daemon-less run still throttles correctly.
    let sock = crate::daemon::client::default_sock_path(paths);
    if let Ok(emit) = crate::daemon::client::should_warn_cargo_debug_default(&sock, repo_path) {
        return emit;
    }
    let db_path = crate::cache_lib::state_db_path(paths);
    crate::cache_lib::state_db::StateDb::open(&db_path)
        .and_then(|db| db.should_emit_cargo_debug_default_warning(repo_path))
        .unwrap_or(true)
}
