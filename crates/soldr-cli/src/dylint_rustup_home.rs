//! The one Rustup home a Dylint execution is provisioned in and runs under
//! (soldr#3567, soldr#3605).
//!
//! Dylint's nightly, its `rustc-dev`/`rust-src`/`llvm-tools-preview`
//! components, and every cross-target `rust-std` are installed by rustup
//! children prepared with `apply_implicit_toolchain_homes`, so they land in
//! Soldr's managed home whenever one exists. The Dylint child, by contrast,
//! took its homes from where its Cargo binary lives (soldr#1799): a host Cargo
//! kept the caller's homes, `RUSTUP_HOME` stayed unset, and every process
//! below it resolved the nightly in `~/.rustup` -- the driver's loader script
//! (`${RUSTUP_HOME:-$HOME/.rustup}`), its `$RUSTUP_HOME/toolchains/
//! $RUSTUP_TOOLCHAIN` sysroot, rustup proxies, and the host `cargo-dylint`
//! Soldr defers to. When that copy of the nightly lacked the target, the check
//! failed with E0463 for `core`/`std` right after Soldr reported installing it.
//!
//! Provisioning and consumption therefore ask this module, and only this
//! module, which home the Dylint scope uses. Ordinary Cargo children keep
//! soldr#1799's binary-located homes untouched: the pin is applied only by
//! `DylintToolchainPlan::apply_to_command`, which also pins
//! `RUSTUP_TOOLCHAIN`, so a default-less managed home never meets a
//! toolchain-less rustup proxy (soldr#1768).

use std::path::{Path, PathBuf};

use crate::binaries::HomeOrigin;
use crate::core::SoldrError;

/// The Rustup home the Dylint nightly, its components and its targets are
/// installed in, and the home every Dylint-scoped child must run under.
///
/// It is the home the installers' rustup children run under, read back from
/// a prepared command: readiness probes used to read the caller's home while
/// `rustup toolchain install` wrote to the managed one, so a successful
/// install was reported as never having happened (soldr#3051).
pub(crate) fn dylint_rustup_home() -> Result<PathBuf, SoldrError> {
    crate::toolchain::effective_rustup_home()
        .ok_or_else(|| SoldrError::Other("could not resolve the Rustup home for Dylint".into()))
}

/// Pin `command` to [`dylint_rustup_home`]. Best-effort: with no resolvable
/// home the child keeps whatever homes it already has.
pub(crate) fn apply_dylint_rustup_home(command: &mut std::process::Command) {
    if let Ok(home) = dylint_rustup_home() {
        command.env(crate::core::RUSTUP_HOME_ENV_VAR, home);
    }
}

/// The `home_origin` a Cargo child's build log records. A Dylint-scoped child
/// whose binary is outside the pinned Dylint home is a [`HomeOrigin::Dylint`]
/// execution rather than the `caller` its binary alone would suggest.
pub(crate) fn child_home_origin(binary: &Path, dylint_scoped: bool) -> Option<HomeOrigin> {
    let origin = crate::binaries::home_origin_for_binary_opt(binary)?;
    if !dylint_scoped {
        return Some(origin);
    }
    Some(dylint_child_home_origin(
        origin,
        dylint_rustup_home().ok().as_deref(),
        crate::core::resolve_rustup_home().as_deref(),
    ))
}

/// [`child_home_origin`]'s decision with every home passed in, so it is
/// testable without the process environment.
fn dylint_child_home_origin(
    binary_origin: HomeOrigin,
    dylint_home: Option<&Path>,
    caller_home: Option<&Path>,
) -> HomeOrigin {
    match (binary_origin, dylint_home) {
        (HomeOrigin::Managed, _) | (_, None) => binary_origin,
        (_, Some(dylint)) if Some(dylint) == caller_home => binary_origin,
        _ => HomeOrigin::Dylint,
    }
}

#[cfg(test)]
#[path = "dylint_rustup_home_tests.rs"]
mod tests;
