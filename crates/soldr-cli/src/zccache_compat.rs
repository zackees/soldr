//! Soldr-owned compatibility adapters for historical `soldr zccache` calls.
//!
//! Do not delegate this argv to an upstream CLI: it can discover and control
//! an unrelated standalone daemon through inherited/default endpoint state.

use crate::cli_args::DaemonSubcommand;
use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::VersionSpec;

pub(crate) async fn run(args: &[String], version: VersionSpec) -> Result<i32, SoldrError> {
    if !matches!(version, VersionSpec::Latest) {
        eprintln!("soldr zccache: version selectors are unsupported; zccache is embedded in soldr");
        return Ok(2);
    }

    let Some(command) = args.first().map(String::as_str) else {
        print_help();
        return Ok(2);
    };
    match command {
        "--help" | "-h" | "help" => {
            print_help();
            Ok(0)
        }
        "--version" | "-V" | "version" => {
            println!(
                "soldr {} (embedded zccache compatibility)",
                env!("CARGO_PKG_VERSION")
            );
            Ok(0)
        }
        "cache-root" => cache_root(&args[1..]),
        "session-end" => session_end(&args[1..]),
        "stop" if args.len() == 1 => {
            crate::run_daemon_command(DaemonSubcommand::Stop).await?;
            Ok(0)
        }
        "rust-plan" => {
            eprintln!("soldr zccache rust-plan is retired; Soldr runs artifact-plan save/restore around `soldr cargo <verb>`.");
            Ok(2)
        }
        _ => Ok(refuse(command)),
    }
}

fn cache_root(args: &[String]) -> Result<i32, SoldrError> {
    if !args.iter().all(|arg| arg == "--json") {
        return Ok(refuse("cache-root"));
    }
    let root = crate::zccache::managed_zccache_cache_dir(&SoldrPaths::new()?)?;
    if args.iter().any(|arg| arg == "--json") {
        println!(
            "{}",
            serde_json::json!({"cache_root": root, "owner": "soldr"})
        );
    } else {
        println!("{}", root.display());
    }
    Ok(0)
}

fn session_end(args: &[String]) -> Result<i32, SoldrError> {
    let mut id = None;
    let mut clear = false;
    let mut json = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Ok(refuse("session-end"));
                };
                if id.replace(value.clone()).is_some() {
                    return Ok(refuse("session-end"));
                }
            }
            "--clear" => clear = true,
            "--json" => json = true,
            value if !value.starts_with('-') && id.is_none() => id = Some(value.to_string()),
            _ => return Ok(refuse("session-end")),
        }
        index += 1;
    }
    crate::cache::run_session_end_command(id, clear, json)?;
    Ok(0)
}

fn print_help() {
    println!("soldr zccache compatibility commands:");
    println!("  cache-root [--json]          Print Soldr's owned cache root");
    println!("  session-end [ID] [--json]    Alias for soldr session-end");
    println!("  stop                         Alias for soldr daemon stop");
    println!("  rust-plan                    Retired; runs automatically with soldr cargo");
}

fn refuse(command: &str) -> i32 {
    eprintln!("soldr zccache: `{command}` is not supported by Soldr's embedded cache.");
    eprintln!("Use `soldr status`, `soldr cache`, `soldr session-end`, or `soldr daemon stop`; `soldr cargo <verb>` owns artifact-plan work.");
    2
}
