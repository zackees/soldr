//! `zccache` — compiled-in zccache CLI trampoline (issue #1368).
//!
//! Before #1368 soldr shipped an *externally downloaded* managed
//! `zccache` binary (the managed-download trio). That download
//! path is gone: soldr already links the whole zccache library in-tree
//! (`_vender/zccache`, the `zccache = { path = ... }` dep), so the CLI
//! surface can be exposed as a normal soldr-cli `[[bin]]` that forwards
//! straight into `zccache::cli::commands::run()`.
//!
//! This binary is what `soldr zccache <args>` execs (see the passthrough
//! in `src/main.rs`). It is the *same* code the standalone `zccache`
//! binary on crates.io ran — argv-driven, returns the zccache CLI's own
//! `ExitCode` — but compiled into soldr's own build so nothing is ever
//! downloaded.

use std::process::ExitCode;

fn main() -> ExitCode {
    zccache::cli::commands::run()
}
