//! Rustup toolchain passthrough commands (`rustc`, `rustfmt`, etc.), the
//! `soldr rustup` front door, and `soldr toolchain install` / `prepare`.
//! Extracted from `main.rs` as part of issue #339.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary, rustup_binary};
use std::time::Duration;
use wait_timeout::ChildExt;

const TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR: &str = "SOLDR_TOOLCHAIN_COMMAND_TIMEOUT_SECS";
const DEFAULT_TOOLCHAIN_COMMAND_TIMEOUT_SECS: u64 = 30 * 60;
const KILLED_TOOLCHAIN_COMMAND_REAP_TIMEOUT_SECS: u64 = 5;

/// Run a rustup-managed toolchain binary with pass-through args.
pub(crate) fn run_toolchain_passthrough(tool: &str, args: &[String]) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary(tool)?;
    let mut command = std::process::Command::new(&binary);
    command.args(args);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, &format!("{tool} passthrough"))?;
    Ok(status.code().unwrap_or(1))
}

/// Run rustc-like toolchain binaries through the same wrapper path Cargo
/// uses when caching is enabled. The wrapper path owns zccache routing and
/// non-cacheable probe bypasses; cache-disabled direct invocations keep the
/// historical passthrough behavior.
pub(crate) fn run_rustc_like(
    tool: &str,
    args: &[String],
    cache_enabled: bool,
) -> Result<i32, SoldrError> {
    if !cache_enabled {
        return run_toolchain_passthrough(tool, args);
    }

    let binary = resolve_toolchain_binary(tool)?;
    let mut raw_args = Vec::with_capacity(args.len() + 2);
    raw_args.push(
        crate::current_soldr_binary()?
            .to_string_lossy()
            .into_owned(),
    );
    raw_args.push(binary.to_string_lossy().into_owned());
    raw_args.extend(args.iter().cloned());
    crate::wrapper::run_rustc_wrapper(&raw_args, crate::startup_profile::WrapperProfile::new())
}

/// Run rustfmt directly for non-cacheable invocations, otherwise route
/// cacheable file-formatting calls through zccache's formatter path.
pub(crate) fn run_rustfmt(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    if !cache_enabled || rustfmt_invocation_bypasses_format_cache(args) {
        return run_toolchain_passthrough("rustfmt", args);
    }

    let rustfmt = resolve_toolchain_binary("rustfmt")?;
    if let Some(zccache) = crate::binaries::non_empty_env_path(crate::TEST_ZCCACHE_BIN_ENV_VAR) {
        let mut command = std::process::Command::new(zccache);
        command.arg(&rustfmt);
        command.args(args);
        crate::binaries::apply_resolved_toolchain_homes(&mut command, &rustfmt);
        apply_zccache_child_env(&mut command)?;
        suppress_windows_console_window(&mut command);
        let status = run_toolchain_command(&mut command, "rustfmt zccache formatter")?;
        return Ok(status.code().unwrap_or(1));
    }

    let cache_root = if let Some(path) =
        crate::binaries::non_empty_env_path(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
    {
        crate::zccache::normalize_path_for_compare(&path)?
    } else {
        let paths = SoldrPaths::new()?;
        crate::zccache::managed_zccache_cache_dir(&paths)?
    };
    std::fs::create_dir_all(&cache_root)?;
    let cwd = std::env::current_dir()?;
    zccache::cli::commands::run_embedded_rustfmt_with_runner(
        &rustfmt,
        args,
        &cwd,
        &cache_root,
        |command| {
            crate::binaries::apply_resolved_toolchain_homes(command, &rustfmt);
            apply_zccache_child_env(command)
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            suppress_windows_console_window(command);
            let status = run_toolchain_command(command, "embedded rustfmt formatter")
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            Ok(status.code().unwrap_or(1))
        },
    )
    .map_err(SoldrError::from)
}

/// Run rustdoc directly. zccache currently has rustc/clippy-driver
/// compile routes and a rustfmt formatter route, but no rustdoc driver
/// route. Cargo doc/doctest still cache their rustc compile units via
/// the cargo front door's `RUSTC_WRAPPER=soldr` injection.
pub(crate) fn run_rustdoc(args: &[String]) -> Result<i32, SoldrError> {
    run_toolchain_passthrough("rustdoc", args)
}

/// Run rust-analyzer itself as a direct rustup-managed LSP passthrough.
/// When caching is enabled, soldr does not wrap the long-lived language
/// server binary. Instead it gives rust-analyzer a scoped child environment
/// so any bare `cargo` / `rustc` / `rustfmt` / `clippy-driver` process it
/// spawns can re-enter Soldr and use the normal zccache cargo front door.
pub(crate) fn run_rust_analyzer(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary("rust-analyzer")?;
    let mut command = std::process::Command::new(&binary);
    command.args(args);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    command.env(
        crate::cache_lib::CACHE_ENABLED_ENV_VAR,
        crate::cache_lib::cache_enabled_env_value(cache_enabled),
    );

    let _shim_guard = if cache_enabled {
        apply_zccache_child_env(&mut command)?;
        maybe_apply_child_shim_dir(&mut command, "rust-analyzer")
    } else {
        None
    };

    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, "rust-analyzer passthrough")?;
    Ok(status.code().unwrap_or(1))
}

fn apply_zccache_child_env(command: &mut std::process::Command) -> Result<(), SoldrError> {
    if std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR).is_none() {
        let paths = SoldrPaths::new()?;
        let cache_dir = crate::zccache::managed_zccache_cache_dir(&paths)?;
        std::fs::create_dir_all(&cache_dir)?;
        command.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, &cache_dir);
        command.env(
            crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
            &cache_dir,
        );
    }
    command.env(
        crate::cache_lib::CACHE_ENABLED_ENV_VAR,
        crate::cache_lib::cache_enabled_env_value(true),
    );
    crate::zccache::ZccacheChildEnv::from_current_process()?.apply_to_command(command);
    Ok(())
}

fn maybe_apply_child_shim_dir(
    command: &mut std::process::Command,
    context: &str,
) -> Option<crate::shim_dir::ShimDirGuard> {
    if !crate::shim_dir::should_install_shims() {
        return None;
    }

    match crate::shim_dir::build_shim_dir() {
        Ok(guard) => {
            crate::shim_dir::apply_to_command(command, &guard.path);
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build child shim dir for {context}; \
                 nested cargo/rustc calls will bypass soldr: {err}"
            );
            None
        }
    }
}

fn rustfmt_invocation_bypasses_format_cache(args: &[String]) -> bool {
    !rustfmt_invocation_has_source_file(args)
}

fn rustfmt_invocation_has_source_file(args: &[String]) -> bool {
    const FLAGS_WITH_VALUE: &[&str] = &[
        "--edition",
        "--config-path",
        "--config",
        "--color",
        "--print-config",
        "--files-with-diff",
        "--file-lines",
    ];

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if matches!(arg, "--help" | "-h" | "--version" | "-V") {
            return false;
        }
        if FLAGS_WITH_VALUE.contains(&arg) {
            index += 2;
            continue;
        }
        if arg.starts_with("--") && arg.contains('=') {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        if arg.ends_with(".rs") {
            return true;
        }
        index += 1;
    }
    false
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
    let status = run_toolchain_command(&mut command, "rustup passthrough")?;
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

/// Prepare the channel needed by the cargo front door using the long
/// toolchain-command timeout. Plugins remain exclusive to prepare/ensure.
pub(crate) fn ensure_cargo_toolchain(explicit_channel: Option<&str>) -> Result<(), SoldrError> {
    let workspace_root = std::env::current_dir()?;
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;
    let channel = explicit_channel
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
        .or_else(|| {
            manifest
                .channel
                .as_deref()
                .map(str::trim)
                .filter(|channel| !channel.is_empty())
        });
    let Some(channel) = channel else {
        return Ok(());
    };

    let mut list = std::process::Command::new(rustup_binary());
    list.args(["toolchain", "list"]);
    apply_implicit_toolchain_homes(&mut list);
    let installed = crate::core::command_output_with_timeout(&mut list, "toolchain list")
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_whitespace().next().is_some_and(|name| {
                    name == channel
                        || name
                            .strip_prefix(channel)
                            .is_some_and(|suffix| suffix.starts_with('-'))
                })
            })
        })
        .unwrap_or(false);
    if !installed {
        let code = rustup_toolchain_install_with_profile(channel, manifest.profile.as_deref())?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "toolchain install {channel} exited with code {code}"
            )));
        }
    }
    if let Some(components) = manifest.components.as_deref() {
        for component in components {
            let code = rustup_component_add(channel, component)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "component add {component} exited with code {code}"
                )));
            }
        }
    }
    if let Some(targets) = manifest.targets.as_deref() {
        for target in targets {
            let code = rustup_target_add(channel, target)?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "target add {target} exited with code {code}"
                )));
            }
        }
    }
    Ok(())
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

    let install_code = rustup_toolchain_install_with_profile(channel, manifest.profile.as_deref())?;
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
/// `$CARGO_HOME`). This is bootstrap/dev-tool acquisition, not a
/// project compile, so it deliberately clears `RUSTC_WRAPPER` /
/// `RUSTC_WORKSPACE_WRAPPER` instead of routing through zccache. The
/// active cargo already honors `rust-toolchain.toml` at exec time, so
/// no explicit channel is passed.
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

    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::binaries::apply_managed_cargo_home_if_available(&mut command);
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(&mut command, &format!("cargo install {name}"))?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_toolchain_install(channel: &str) -> Result<i32, SoldrError> {
    rustup_toolchain_install_with_profile(channel, None)
}

fn rustup_toolchain_install_with_profile(
    channel: &str,
    profile: Option<&str>,
) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args([
        "toolchain",
        "install",
        channel,
        "--profile",
        profile.unwrap_or("minimal"),
        "--no-self-update",
    ]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status =
        run_toolchain_command(&mut command, &format!("rustup toolchain install {channel}"))?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_component_add(channel: &str, component: &str) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "add", "--toolchain", channel, component]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(
        &mut command,
        &format!("rustup component add {component} --toolchain {channel}"),
    )?;
    Ok(status.code().unwrap_or(1))
}

fn rustup_target_add(channel: &str, target: &str) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "add", "--toolchain", channel, target]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = run_toolchain_command(
        &mut command,
        &format!("rustup target add {target} --toolchain {channel}"),
    )?;
    Ok(status.code().unwrap_or(1))
}

fn toolchain_command_timeout() -> Duration {
    std::env::var(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_TOOLCHAIN_COMMAND_TIMEOUT_SECS))
}

fn run_toolchain_command(
    command: &mut std::process::Command,
    context: &str,
) -> Result<std::process::ExitStatus, SoldrError> {
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("failed to invoke {context}: {err}")))?;
    let timeout = toolchain_command_timeout();
    match child
        .wait_timeout(timeout)
        .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
    {
        Some(status) => Ok(status),
        None => {
            let kill_result = child.kill();
            let reap_result = child.wait_timeout(Duration::from_secs(
                KILLED_TOOLCHAIN_COMMAND_REAP_TIMEOUT_SECS,
            ));
            let timeout_secs = timeout.as_secs();
            let mut message = format!(
                "{context} timed out after {timeout_secs} seconds \
                 (set {TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR} to override)"
            );
            match kill_result {
                Ok(()) => message.push_str("; killed child process"),
                Err(err) => message.push_str(&format!("; kill failed: {err}")),
            }
            match reap_result {
                Ok(Some(_)) => {}
                Ok(None) => message.push_str(&format!(
                    "; process did not exit within {KILLED_TOOLCHAIN_COMMAND_REAP_TIMEOUT_SECS} seconds after kill"
                )),
                Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
            }
            Err(SoldrError::Other(message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    crate::timed_test!(toolchain_command_timeout_uses_positive_env_override_only, {
        let _lock = ENV_LOCK.lock().expect("env lock");
        {
            let _guard = EnvVarGuard::set(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR, "23");
            assert_eq!(toolchain_command_timeout(), Duration::from_secs(23));
        }
        for value in ["", "0", "-1", "abc"] {
            let _guard = EnvVarGuard::set(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR, value);
            assert_eq!(
                toolchain_command_timeout(),
                Duration::from_secs(DEFAULT_TOOLCHAIN_COMMAND_TIMEOUT_SECS),
                "invalid override {value:?} should use default"
            );
        }
        let _guard = EnvVarGuard::remove(TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR);
        assert_eq!(
            toolchain_command_timeout(),
            Duration::from_secs(DEFAULT_TOOLCHAIN_COMMAND_TIMEOUT_SECS)
        );
    });
}
