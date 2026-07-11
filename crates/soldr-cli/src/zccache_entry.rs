//! `zccache` multicall entrypoint for the vendored CLI.

use std::process::ExitCode;

const ALLOWED_SUBCOMMANDS: &[&str] = &["rust-plan", "session-end", "stop", "cache-root"];

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

/// Run the zccache surface selected by the `zccache` argv[0] alias.
pub fn run() -> ExitCode {
    std::env::set_var("ZCCACHE_NO_SPAWN", "1");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(subcommand) = first_subcommand(&args) {
        if !ALLOWED_SUBCOMMANDS.contains(&subcommand.as_str()) {
            return refuse(subcommand);
        }
    }
    run_cli()
}

#[cfg(windows)]
fn run_cli() -> ExitCode {
    match std::thread::Builder::new()
        .name("zccache-cli".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(zccache::cli::commands::run)
    {
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
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

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(subcommand_gate_finds_first_non_flag_argument, {
        assert_eq!(first_subcommand(&["--version".into()]), None);
        assert_eq!(
            first_subcommand(&["--json".into(), "status".into()]).map(String::as_str),
            Some("status")
        );
    });

    crate::timed_test!(refused_subcommand_preserves_usage_exit_code, {
        assert_eq!(refuse("status"), ExitCode::from(2));
    });
}
