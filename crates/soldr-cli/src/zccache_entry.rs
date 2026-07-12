//! In-process `soldr zccache` entrypoint for the vendored CLI.

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

/// Run the daemon-free zccache maintenance surface inside this soldr process.
pub fn run_with_args(args: &[String]) -> ExitCode {
    std::env::set_var("ZCCACHE_NO_SPAWN", "1");
    if let Some(subcommand) = first_subcommand(args) {
        if !ALLOWED_SUBCOMMANDS.contains(&subcommand.as_str()) {
            return refuse(subcommand);
        }
    }
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("zccache".to_string());
    full_args.extend(args.iter().cloned());
    run_cli(full_args)
}

fn run_cli(args: Vec<String>) -> ExitCode {
    let builder = std::thread::Builder::new()
        .name("zccache-cli".to_string())
        .stack_size(8 * 1024 * 1024);
    match builder.spawn(move || zccache::cli::commands::run_with_args(&args)) {
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
        Err(err) => {
            eprintln!("zccache: failed to start CLI thread: {err}");
            ExitCode::FAILURE
        }
    }
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

    crate::timed_test!(in_process_cache_root_dispatch_succeeds, {
        assert_eq!(
            run_with_args(&["cache-root".into(), "--json".into()]),
            ExitCode::SUCCESS
        );
        assert_eq!(std::env::var("ZCCACHE_NO_SPAWN").as_deref(), Ok("1"));
    });
}
