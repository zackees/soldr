//! `zccache` — compiled-in zccache CLI trampoline (issues #1368 + #1467).
//!
//! Before #1368 soldr shipped an *externally downloaded* managed
//! `zccache` binary (the managed-download trio). That download
//! path is gone: soldr already links the whole zccache library in-tree
//! (`_vender/zccache`, the `zccache = { path = ... }` dep), so the CLI
//! surface can be exposed as a normal soldr-cli `[[bin]]` that forwards
//! into `zccache::cli::commands::run()`.
//!
//! This binary is what `soldr zccache <args>` execs (see the passthrough
//! in `src/main.rs`).
//!
//! # Embedded-only gate (soldr#1467)
//!
//! soldr's build cache runs as an **embedded service inside soldr-daemon**
//! — a standalone `zccache-daemon` process must never spawn on a soldr
//! box. The upstream CLI lazily spawns one from a dozen subcommands
//! (`start`, `session-start`, `status`, `warm`, `wrap`/`cc`, `exec`, `fp`,
//! ...), so the trampoline only passes through the daemon-free
//! subcommands in [`ALLOWED_SUBCOMMANDS`] and hard-errors the rest,
//! pointing at the embedded equivalents. As defense-in-depth it also
//! exports `ZCCACHE_NO_SPAWN=1` (zccache#982) so even an allowlisted
//! subcommand — or a future drift in the allowlist — can never spawn.
//!
//! # Windows stack parity
//!
//! Upstream's `zccache` binary runs the CLI on a dedicated 8 MiB-stack
//! thread on Windows because the CLI needs more than the 1 MiB default
//! main-thread stack. The pre-#1467 trampoline called `run()` on the
//! main thread and **overflowed its stack on every invocation** in debug
//! builds; the runner below mirrors the upstream pattern.

use std::process::ExitCode;

/// Daemon-free upstream subcommands: these never reach the zccache CLI's
/// lazy daemon-spawn chain (`ensure_daemon` → `spawn_daemon`), verified
/// against the spawn-site audit in soldr#1467.
const ALLOWED_SUBCOMMANDS: &[&str] = &["rust-plan", "session-end", "stop", "cache-root"];

/// First non-flag argument = the subcommand the upstream clap parser
/// would dispatch on. `None` for flag-only invocations (`--help`,
/// `--version`), which are safe to pass through.
fn first_subcommand(args: &[String]) -> Option<&String> {
    args.iter().find(|arg| !arg.starts_with('-'))
}

fn refuse(subcommand: &str) -> ExitCode {
    eprintln!(
        "soldr zccache: `{subcommand}` is not available under soldr — the build cache runs as \
         an embedded service inside soldr-daemon, and standalone zccache-daemon processes are \
         never spawned (soldr#1467)."
    );
    eprintln!(
        "passthrough subcommands: {} (plus --help / --version)",
        ALLOWED_SUBCOMMANDS.join(", ")
    );
    eprintln!(
        "equivalents: `soldr status` / `soldr cache` show cache state; `soldr cargo <verb>` \
         builds use the embedded cache automatically. Embedded parity for the remaining \
         subcommands is tracked in zackees/zccache#905."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    // zccache#982 defense-in-depth: the vendored CLI refuses to spawn a
    // standalone daemon even if a subcommand slips past the allowlist.
    std::env::set_var("ZCCACHE_NO_SPAWN", "1");

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(subcommand) = first_subcommand(&args) {
        if !ALLOWED_SUBCOMMANDS.contains(&subcommand.as_str()) {
            return refuse(subcommand);
        }
    }
    run_cli()
}

/// Windows: 8 MiB CLI thread, mirroring upstream `src/bin/zccache.rs` —
/// the 1 MiB default main-thread stack overflows inside the CLI.
#[cfg(windows)]
fn run_cli() -> ExitCode {
    match std::thread::Builder::new()
        .name("zccache-cli".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(zccache::cli::commands::run)
    {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            Err(_) => ExitCode::FAILURE,
        },
        Err(err) => {
            eprintln!("zccache: failed to start CLI thread: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn run_cli() -> ExitCode {
    zccache::cli::commands::run()
}
