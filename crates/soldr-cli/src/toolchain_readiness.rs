//! soldr#2618 — explicit pinned-toolchain provisioning for the blessed
//! build path.
//!
//! Nothing on the blessed `soldr build --target <T>` path installed the
//! pinned toolchain explicitly: `bootstrap_rustup` runs rustup-init with
//! `--default-toolchain none`, so on a fresh root the toolchain was
//! installed implicitly by whichever child process first invoked a rustup
//! proxy — usually the soldr#1543 dependency prefetch, spawned
//! concurrently with SDK preparation. `rustup target add` (which does not
//! auto-install) then raced that in-flight install and failed with
//! `Missing manifest in toolchain`, soldr exited, and the prefetch child
//! was reaped mid-extraction via `kill_on_drop` — leaving a toolchain
//! directory with some component manifests but no rustc and no channel
//! manifest. rustup proxies treat directory-exists as installed, so every
//! later run kept failing the same way: a permanent wedge.
//!
//! This module makes the install explicit and sequential
//! ([`ensure_pinned_toolchain_installed`]), and self-heals the wedged
//! partial state left behind by older soldr versions or an interrupted
//! bootstrap (uninstall + reinstall when the toolchain directory exists
//! without a rustc). The probe, the install, and
//! [`crate::prepare_cmd::rustup_add_target`] all force the same
//! `RUSTUP_HOME` (caller env if set, soldr-managed otherwise), so the
//! toolchain the probe inspects is the toolchain the install writes and
//! the target add mutates.

use crate::core::{run_installer_command, InstallerWatchdogConfig, SoldrError, SoldrPaths};
use std::path::{Path, PathBuf};

/// Where the pinned toolchain stands in the effective `RUSTUP_HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolchainReadiness {
    /// rustc is present — the toolchain is usable.
    Ready,
    /// No toolchain directory at all — plain install.
    Missing,
    /// Toolchain directory exists but has no rustc: the signature of an
    /// interrupted or raced install. Requires uninstall + reinstall,
    /// because rustup proxies treat directory-exists as installed and
    /// will never repair it on their own.
    Partial,
}

/// Directory name rustup gives a toolchain: bare channels
/// (`1.95.0`, `stable`, `nightly-2026-01-01`) get `-<host>` appended;
/// an already host-qualified channel maps to itself.
pub(crate) fn toolchain_dir_name(channel: &str, host: &str) -> String {
    if host.is_empty() || channel.ends_with(host) {
        channel.to_string()
    } else {
        format!("{channel}-{host}")
    }
}

pub(crate) fn classify(dir_exists: bool, rustc_exists: bool) -> ToolchainReadiness {
    match (dir_exists, rustc_exists) {
        (_, true) => ToolchainReadiness::Ready,
        (true, false) => ToolchainReadiness::Partial,
        (false, false) => ToolchainReadiness::Missing,
    }
}

fn rustc_filename() -> &'static str {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        "rustc.exe"
    } else {
        "rustc"
    }
}

/// Filesystem-only probe against an explicit home — no process spawns, so
/// it can never itself trigger a rustup proxy auto-install.
pub(crate) fn probe_toolchain_state(
    rustup_home: &Path,
    channel: &str,
    host: &str,
) -> ToolchainReadiness {
    let dir = rustup_home
        .join("toolchains")
        .join(toolchain_dir_name(channel, host));
    let rustc = dir.join("bin").join(rustc_filename());
    classify(dir.is_dir(), rustc.is_file())
}

/// The `RUSTUP_HOME` that [`crate::prepare_cmd::rustup_add_target`] forces
/// on its child: the caller's env value when set, the soldr-managed home
/// otherwise.
fn effective_rustup_home() -> Result<PathBuf, SoldrError> {
    if let Some(home) = std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR) {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    let paths = SoldrPaths::new()?;
    Ok(crate::fetch::managed_rustup_home(&paths))
}

fn pinned_channel() -> Result<Option<String>, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    Ok(crate::core::read_rust_toolchain_manifest(&workspace_root)?.channel)
}

/// Ensure the pinned toolchain (from `rust-toolchain.toml`) is fully
/// installed in the effective `RUSTUP_HOME` before any rustup subcommand
/// that requires it runs. No-op when no channel is pinned or the
/// toolchain is already usable; installs sequentially otherwise; heals a
/// partial toolchain by uninstalling the corrupt directory first.
pub(crate) fn ensure_pinned_toolchain_installed() -> Result<(), SoldrError> {
    let Some(channel) = pinned_channel()? else {
        return Ok(());
    };
    let rustup_home = effective_rustup_home()?;
    match probe_toolchain_state(&rustup_home, &channel, crate::pyo3_detect::host_triple()) {
        ToolchainReadiness::Ready => Ok(()),
        ToolchainReadiness::Missing => {
            if !crate::core::quiet::diagnostics_suppressed() {
                eprintln!(
                    "soldr: pinned toolchain {channel} is not installed; \
                     installing it before continuing (soldr#2618)"
                );
            }
            run_pinned_rustup(&["toolchain", "install", &channel], "toolchain-install")
        }
        ToolchainReadiness::Partial => {
            if !crate::core::quiet::diagnostics_suppressed() {
                eprintln!(
                    "soldr: pinned toolchain {channel} is partially installed \
                     (no rustc — an earlier install was interrupted); \
                     reinstalling it (soldr#2618)"
                );
            }
            // Best-effort cleanup: the partial directory is unusable, and
            // leaving it makes rustup skip the reinstall.
            let _ = run_pinned_rustup(&["toolchain", "uninstall", &channel], "toolchain-repair");
            run_pinned_rustup(&["toolchain", "install", &channel], "toolchain-install")
        }
    }
}

/// Run a rustup subcommand under the same forced homes as
/// [`crate::prepare_cmd::rustup_add_target`], so probe, install, and
/// target add all act on one toolchain store.
fn run_pinned_rustup(args: &[&str], kind: &str) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let rustup = crate::binaries::rustup_binary();
    let mut command = std::process::Command::new(rustup);
    let mut full_args: Vec<&str> = args.to_vec();
    if args.first() == Some(&"toolchain") && args.get(1) == Some(&"install") {
        full_args.extend(["--profile", "minimal", "--no-self-update"]);
    }
    command.args(&full_args);
    command.env(
        crate::core::CARGO_HOME_ENV_VAR,
        std::env::var_os(crate::core::CARGO_HOME_ENV_VAR)
            .unwrap_or_else(|| crate::fetch::managed_cargo_home(&paths).into_os_string()),
    );
    command.env(
        crate::core::RUSTUP_HOME_ENV_VAR,
        std::env::var_os(crate::core::RUSTUP_HOME_ENV_VAR)
            .unwrap_or_else(|| crate::fetch::managed_rustup_home(&paths).into_os_string()),
    );
    crate::core::suppress_windows_console_window(&mut command);
    let context = format!("rustup {}", full_args.join(" "));
    let status = run_installer_command(
        &mut command,
        &context,
        kind,
        InstallerWatchdogConfig::from_env(crate::toolchain::TOOLCHAIN_COMMAND_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!("{context} exited with {status}")));
    }
    Ok(())
}

#[cfg(test)]
#[path = "toolchain_readiness_tests.rs"]
mod tests;
