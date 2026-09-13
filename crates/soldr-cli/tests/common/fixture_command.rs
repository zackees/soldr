//! soldr#3203: fixture `soldr` commands that cannot build into the test suite's
//! own `target/`.
//!
//! Soldr resolves Cargo's target directory as Cargo does, so a fixture run from
//! the crate directory -- a test's default working directory -- lands in the
//! repository's own `target/`, where the no-cache preflight and cleanup hooks
//! would modify live test binaries. The nextest wrapper's
//! `SOLDR_TEST_FORBID_TARGET_CONTAINING` tripwire refuses such a build; these
//! are the two ways out of it.

use std::path::Path;
use std::process::Command;

/// [`super::isolated_soldr_command`] run from `dir`, a temporary directory with
/// no `Cargo.toml`, so no target hook has a target to touch.
pub(crate) fn isolated_soldr_command_in(dir: &Path) -> Command {
    let mut command = super::isolated_soldr_command();
    command.current_dir(dir);
    command
}

/// [`super::isolated_soldr_command`] building into its own `target_dir`, for a
/// fixture whose unmediated build must prepare a real target. Set after the
/// scrub, which removes an inherited `CARGO_TARGET_DIR`.
pub(crate) fn isolated_soldr_command_with_target(target_dir: &Path) -> Command {
    let mut command = super::isolated_soldr_command();
    command.env("CARGO_TARGET_DIR", target_dir);
    command
}
