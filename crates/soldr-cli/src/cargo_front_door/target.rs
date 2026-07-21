//! Target-triple resolution and `SOLDR_LINKER` injection on the cargo
//! subprocess command line.
//!
//! Lifted out of the front-door entry point so the policy can be unit-
//! tested in isolation and re-used by other dispatch paths in the
//! future without dragging the full `run_cargo_front_door` body along.

use crate::core::{SoldrError, SoldrPaths};
use crate::{linker, LINKER_ENV_VAR};

use super::subcommand::{cargo_args_specify_target, cargo_args_target_value};

pub(super) fn default_cargo_build_target(args: &[String]) -> Result<Option<String>, SoldrError> {
    if !cfg!(windows) {
        return Ok(None);
    }
    if cargo_args_specify_target(args) || std::env::var_os("CARGO_BUILD_TARGET").is_some() {
        return Ok(None);
    }

    Ok(Some(crate::core::TargetTriple::detect()?.triple()))
}

/// Return the target Cargo will build when soldr can know it without
/// falling back to host auto-detection.
///
/// `default_cargo_build_target` has a narrower job: inject Windows' native
/// MSVC default only when the user did not pass a target. Native C caching
/// needs the explicit target too, otherwise cross builds only get a generic
/// `CC` wrapper and cc-rs can fall back to the host compiler.
pub(super) fn known_cargo_build_target(
    args: &[String],
    defaulted_target: Option<&str>,
) -> Option<String> {
    if let Some(target) = defaulted_target {
        return Some(target.to_string());
    }
    if let Some(target) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = target.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    cargo_args_target_value(args)
}

/// Apply the `SOLDR_LINKER` / `config.toml linker = ...` override (issue
/// #285) to the cargo subprocess command.
///
/// The active target triple is resolved in the same order as cargo:
/// 1. an explicit `CARGO_BUILD_TARGET` injected by `default_cargo_build_target`,
/// 2. a `CARGO_BUILD_TARGET` already in the parent env,
/// 3. an `--target` flag inside `args`,
/// 4. the auto-detected host triple from `TargetTriple::detect()`.
pub(super) fn apply_linker_override(
    command: &mut std::process::Command,
    args: &[String],
    explicit_target: Option<&str>,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    let config = paths
        .load_config()
        .map_err(|error| SoldrError::Other(error.to_string()))?;
    let choice = linker::from_env_and_config(
        std::env::var_os(LINKER_ENV_VAR).as_deref(),
        config.linker.as_deref(),
    )?;
    if matches!(choice, linker::LinkerChoice::Default) {
        // Fast-path: skip target detection entirely when there is nothing
        // to inject. Keeps `soldr cargo` no-ops on platforms where target
        // detection might fail or be slow.
        return Ok(());
    }

    let target = resolve_active_target_triple(args, explicit_target)?;
    let injection = linker::resolve_for_target(choice, &target)?;
    let prefix = linker::cargo_target_env_prefix(&target);
    let linker_key = format!("CARGO_TARGET_{prefix}_LINKER");
    let rustflags_key = format!("CARGO_TARGET_{prefix}_RUSTFLAGS");
    // A target-scoped linker/rustflags value is more specific than soldr's
    // convenience linker selection. This matters for cross builds: the
    // workflow may already have installed a target-aware Zig wrapper while
    // SOLDR_LINKER=fast is inherited from setup-soldr. Do not replace an
    // explicit target toolchain with the host clang/LLD fallback.
    if let Some(linker_path) = injection.linker {
        if !effective_command_env_is_non_empty(command, &linker_key) {
            command.env(&linker_key, linker_path);
        }
    }
    if let Some(rustflags) = injection.rustflags {
        if !effective_command_env_is_non_empty(command, &rustflags_key) {
            command.env(&rustflags_key, rustflags);
        }
    }
    Ok(())
}

/// Return whether a non-empty value will reach the cargo child. Values set
/// directly on `command` take precedence over the parent's environment,
/// including an explicit removal. This helper keeps linker injection from
/// clobbering a target-specific cross toolchain with SOLDR_LINKER's host
/// default.
fn effective_command_env_is_non_empty(command: &std::process::Command, key: &str) -> bool {
    if let Some(value) = command
        .get_envs()
        .find(|(candidate, _)| *candidate == std::ffi::OsStr::new(key))
        .map(|(_, value)| value)
    {
        return value.is_some_and(|value| !value.is_empty());
    }
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

fn resolve_active_target_triple(
    args: &[String],
    explicit_target: Option<&str>,
) -> Result<String, SoldrError> {
    if let Some(target) = explicit_target {
        return Ok(target.to_string());
    }
    if let Some(target) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = target.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    if let Some(target) = cargo_args_target_value(args) {
        return Ok(target);
    }
    Ok(crate::core::TargetTriple::detect()?.triple())
}
