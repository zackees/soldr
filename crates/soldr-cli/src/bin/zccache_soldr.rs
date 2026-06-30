//! `zccache-soldr` — dedicated RUSTC_WRAPPER shim that forwards every
//! rustc invocation to soldr-daemon's embedded zccache service over the
//! `Request::Compile` IPC verb (issue #1081).
//!
//! Why a dedicated binary instead of reusing the `soldr` mode-detect
//! path:
//!
//!   * **Stable install path.** A version-tagged `soldr.exe` moves on
//!     every release, which invalidates cargo's RUSTC_WRAPPER
//!     fingerprint and forces a downstream cache rebuild. A
//!     stable-pathed `~/.soldr/bin/zccache-soldr.exe` (or sibling-to-
//!     soldr install slot) lets cargo's fingerprint stay stable across
//!     soldr upgrades.
//!   * **No mode-dispatch surface.** The main `soldr` binary inspects
//!     argv[1] for "is this a path to a known toolchain tool?" — a
//!     small but non-zero cost on every rustc invocation. The shim has
//!     exactly one job and skips that decision tree.
//!   * **No standalone zccache.exe required.** The embedded-zccache
//!     architecture (issue #977 / #980 L1) puts the whole zccache
//!     library inside soldr-daemon. The shim talks to the daemon over
//!     IPC; we ship NO `zccache.exe` / `zccache-daemon.exe` /
//!     `zccache-fp.exe` separately. (See issue #1081 for the bundling-
//!     removal follow-up.)
//!
//! ## Argv contract
//!
//! cargo invokes RUSTC_WRAPPER as `[wrapper_path, rustc_path,
//! ...rustc_args]`. After we strip argv[0] (the wrapper's own path),
//! we get `[rustc_path, ...rustc_args]` — the shape
//! `compile_dispatch::dispatch_compile` expects.
//!
//! ## Hang-safety
//!
//! The dispatch honors `SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS` (default
//! 30 s). The daemon side drops the in-flight compile if the shim
//! disconnects (PR #1078). Both halves together guarantee the shim
//! never wedges cargo on a stuck daemon.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Strip argv[0] (the shim's own path); send the rest as the
    // [rustc_path, ...rustc_args] tail.
    let rustc_argv: Vec<String> = std::env::args().skip(1).collect();
    if rustc_argv.is_empty() {
        eprintln!(
            "zccache-soldr: missing rustc-path argument (RUSTC_WRAPPER contract is \
             `[wrapper_path, rustc_path, ...rustc_args]`)"
        );
        return ExitCode::from(2);
    }

    match soldr_cli::compile_dispatch::dispatch_compile(
        &rustc_argv,
        std::io::stdout(),
        std::io::stderr(),
    ) {
        Ok(code) => exit_code_from_i32(code),
        Err(err) => {
            eprintln!("zccache-soldr: dispatch failed: {err}");
            // Use 101 so cargo treats it as a compile failure (matches
            // rustc's own panic exit code convention).
            ExitCode::from(101)
        }
    }
}

/// Map a rustc-style i32 exit code into the [0, 255] u8 range used by
/// `ExitCode`. Negative codes (signal-style on Unix) collapse to 1 so
/// the parent cargo sees a generic failure rather than a confusing
/// truncated negative.
fn exit_code_from_i32(code: i32) -> ExitCode {
    if (0..=255).contains(&code) {
        ExitCode::from(code as u8)
    } else {
        ExitCode::from(1)
    }
}
