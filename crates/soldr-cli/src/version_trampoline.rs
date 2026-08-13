//! The `--as <version>` trampoline: hand this invocation to a different
//! soldr version.
//!
//! Split out of `soldr_main.rs` for soldr#2024 — that file is well over
//! the line ceiling and the ratchet correctly refused to let it grow.
//! This is a self-contained unit: resolve the requested version, fetch or
//! reuse it, and exec it with our argv and stdio.
//!
//! Distinct from [`crate::trampoline`], which is the `soldr cargo run`
//! binary trampoline (#342) and shares only the name.

use crate::core::{suppress_windows_console_window, SoldrError};
use crate::fetch::VersionSpec;

/// Sentinel that the currently-running soldr was itself invoked by another
/// soldr through `--as`. Prevents infinite hand-offs.
pub(crate) const SOLDR_TRAMPOLINING_ENV_VAR: &str = "SOLDR_TRAMPOLINING";

pub(crate) async fn run(version: &str, args: &[String]) -> Result<i32, SoldrError> {
    if let Ok(prior) = std::env::var(SOLDR_TRAMPOLINING_ENV_VAR) {
        return Err(SoldrError::Other(format!(
            "refusing to trampoline again: this process was already reached via `--as` from soldr {prior}. Drop the inner --as flag."
        )));
    }

    eprintln!("soldr: trampolining to soldr@{version}...");
    let result = crate::fetch::fetch_tool(
        "soldr",
        &VersionSpec::Exact(crate::cli_dispatch::normalize_version(version)),
    )
    .await?;

    if result.cached {
        eprintln!(
            "soldr: using cached soldr v{} at {}",
            result.version,
            result.binary_path.display()
        );
    } else {
        eprintln!(
            "soldr: downloaded soldr v{} to {}",
            result.version,
            result.binary_path.display()
        );
    }

    let mut command = std::process::Command::new(&result.binary_path);
    command
        .args(args)
        .env(SOLDR_TRAMPOLINING_ENV_VAR, env!("CARGO_PKG_VERSION"));
    suppress_windows_console_window(&mut command);
    // soldr#2024: the pinned soldr inherits our stdio and owns the
    // outcome; this process only relays its exit code.
    crate::exit_guard::mark_spoke();

    // Unix execs (replacing this image); Windows spawns and waits. Only
    // the failure path returns here on Unix.
    match crate::platform::process::spawn::exec_or_status(&mut command) {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(err) => Err(SoldrError::Other(format!(
            "failed to exec soldr v{} at {}: {err}",
            result.version,
            result.binary_path.display()
        ))),
    }
}
