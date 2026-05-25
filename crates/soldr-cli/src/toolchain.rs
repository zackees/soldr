//! Rustup toolchain passthrough commands (`rustc`, `rustfmt`, etc.), the
//! `soldr rustup` front door, and `soldr toolchain install` / `prepare`.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{suppress_windows_console_window, SoldrError};
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary, rustup_binary};

/// Run a rustup-managed toolchain binary with pass-through args.
pub(crate) fn run_toolchain_passthrough(tool: &str, args: &[String]) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary(tool)?;
    let mut command = std::process::Command::new(binary);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

/// Drop-in passthrough for `soldr rustup ...`.
///
/// Most invocations forward verbatim. The one exception is the "scoped"
/// pin: when the first positional argument is `target` or `component`
/// (the two rustup subcommands that mutate per-toolchain state) AND
/// `rust-toolchain.toml` declares a `channel`, soldr inserts
/// `--toolchain <channel>` immediately after the user's first positional
/// argument so the call lands on the pinned toolchain rather than the
/// rustup default. If the user already supplied `--toolchain` anywhere,
/// the injection is skipped.
pub(crate) fn run_rustup_passthrough(args: &[String]) -> Result<i32, SoldrError> {
    let final_args = scope_rustup_args_to_pin(args)?;
    let mut command = std::process::Command::new(rustup_binary());
    command.args(&final_args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn scope_rustup_args_to_pin(args: &[String]) -> Result<Vec<String>, SoldrError> {
    // Find the first non-flag positional. Anything before it (e.g.
    // `--verbose`) is preserved in place.
    let mut first_positional: Option<usize> = None;
    for (idx, arg) in args.iter().enumerate() {
        if !arg.starts_with('-') {
            first_positional = Some(idx);
            break;
        }
    }

    let Some(first_positional) = first_positional else {
        return Ok(args.to_vec());
    };

    let subcommand = args[first_positional].as_str();
    if subcommand != "target" && subcommand != "component" {
        return Ok(args.to_vec());
    }

    if rustup_args_specify_toolchain(args) {
        return Ok(args.to_vec());
    }

    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;
    let Some(channel) = manifest.channel else {
        return Ok(args.to_vec());
    };

    // Inject `--toolchain <channel>` after the subcommand/verb pair so a
    // call like `target add x86_64-unknown-linux-musl` becomes
    // `target add --toolchain <channel> x86_64-unknown-linux-musl`.
    // The verb is the next non-flag positional after `target`/`component`.
    let mut insertion_idx = first_positional + 1;
    for (offset, arg) in args[first_positional + 1..].iter().enumerate() {
        if !arg.starts_with('-') {
            insertion_idx = first_positional + 1 + offset + 1;
            break;
        }
    }

    let mut out = Vec::with_capacity(args.len() + 2);
    out.extend(args[..insertion_idx].iter().cloned());
    out.push("--toolchain".to_string());
    out.push(channel);
    out.extend(args[insertion_idx..].iter().cloned());
    Ok(out)
}

fn rustup_args_specify_toolchain(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--toolchain" || arg.starts_with("--toolchain="))
}

/// Implementation of `soldr toolchain install`.
pub(crate) fn run_toolchain_install() -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;
    let Some(channel) = manifest.channel.as_deref() else {
        eprintln!(
            "soldr: no rust-toolchain.toml channel found; nothing to install. \
             Create rust-toolchain.toml with a `[toolchain] channel = \"<version>\"` entry."
        );
        return Ok(0);
    };

    rustup_toolchain_install(channel)
}

/// Implementation of `soldr toolchain prepare`.
pub(crate) fn run_toolchain_prepare() -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;
    let Some(channel) = manifest.channel.as_deref() else {
        eprintln!(
            "soldr: no rust-toolchain.toml channel found; nothing to prepare. \
             Create rust-toolchain.toml with a `[toolchain] channel = \"<version>\"` entry."
        );
        return Ok(0);
    };

    let (code, _summary) = run_prepare_inner(channel, &manifest)?;
    Ok(code)
}

/// Summary of what `prepare` actually did, used by
/// [`crate::toolchain_ensure`] to populate the JSON payload.
#[derive(Debug, Default)]
pub(crate) struct PrepareSummary {
    pub components_added: Vec<String>,
    pub targets_added: Vec<String>,
    pub plugins_installed: Vec<String>,
}

/// Shared inner driver for `prepare` / `ensure`. Returns the rustup /
/// cargo exit code (0 on success) plus a [`PrepareSummary`] of what was
/// actually attempted (we report every declared component/target/plugin
/// rustup didn't error on; for now we cannot cheaply diff "already
/// installed" from "newly installed" without parsing rustup output,
/// which is fragile across versions).
pub(crate) fn run_prepare_inner(
    channel: &str,
    manifest: &crate::core::RustToolchainManifest,
) -> Result<(i32, PrepareSummary), SoldrError> {
    let mut summary = PrepareSummary::default();

    let install_code = rustup_toolchain_install(channel)?;
    if install_code != 0 {
        return Ok((install_code, summary));
    }

    if let Some(components) = manifest.components.as_deref() {
        for component in components {
            let code = rustup_component_add(channel, component)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.components_added.push(component.clone());
        }
    }

    if let Some(targets) = manifest.targets.as_deref() {
        for target in targets {
            let code = rustup_target_add(channel, target)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.targets_added.push(target.clone());
        }
    }

    if let Some(soldr_section) = manifest.soldr.as_ref() {
        if !soldr_section.plugins.is_empty() {
            for (name, spec) in &soldr_section.plugins {
                let code = cargo_install_plugin(name, spec)?;
                if code != 0 {
                    return Ok((code, summary));
                }
                summary
                    .plugins_installed
                    .push(format_plugin_label(name, spec));
            }
        }
    }

    Ok((0, summary))
}

/// Format a plugin entry as `name` or `name@version` for the JSON
/// payload. Detailed specs with no version collapse to `name`; bare
/// `*` specs also collapse to `name`.
fn format_plugin_label(name: &str, spec: &crate::core::PluginSpec) -> String {
    let version = match spec {
        crate::core::PluginSpec::Version(v) => Some(v.as_str()),
        crate::core::PluginSpec::Detailed { version, .. } => version.as_deref(),
    };
    let trimmed = version
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "*");
    match trimmed {
        Some(v) => format!("{name}@{v}"),
        None => name.to_string(),
    }
}

/// Install one plugin declared under `[soldr.plugins]` via the
/// resolved cargo binary (so installs respect soldr-managed
/// `$CARGO_HOME`). We deliberately do NOT route through the rustc
/// wrapper machinery — that path is meant for compile units, not
/// dev-tool installation. The active cargo already honors
/// `rust-toolchain.toml` at exec time, so no explicit channel is
/// passed.
fn cargo_install_plugin(name: &str, spec: &crate::core::PluginSpec) -> Result<i32, SoldrError> {
    let cargo = resolve_toolchain_binary("cargo")?;
    let mut command = std::process::Command::new(&cargo);
    command.arg("install").arg(name);

    let (version, locked, features, no_default_features) = match spec {
        crate::core::PluginSpec::Version(value) => (Some(value.as_str()), None, None, None),
        crate::core::PluginSpec::Detailed {
            version,
            locked,
            features,
            no_default_features,
        } => (
            version.as_deref(),
            *locked,
            features.as_deref(),
            *no_default_features,
        ),
    };

    if let Some(version) = version {
        let trimmed = version.trim();
        if !trimmed.is_empty() && trimmed != "*" {
            command.arg("--version").arg(trimmed);
        }
    }
    if locked == Some(true) {
        command.arg("--locked");
    }
    if no_default_features == Some(true) {
        command.arg("--no-default-features");
    }
    if let Some(features) = features {
        let joined = features.join(",");
        if !joined.is_empty() {
            command.arg("--features").arg(joined);
        }
    }

    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_toolchain_install(channel: &str) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args([
        "toolchain",
        "install",
        channel,
        "--profile",
        "minimal",
        "--no-self-update",
    ]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_component_add(channel: &str, component: &str) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "add", "--toolchain", channel, component]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_target_add(channel: &str, target: &str) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "add", "--toolchain", channel, target]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}
