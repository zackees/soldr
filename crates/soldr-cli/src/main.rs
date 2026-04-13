use clap::{Parser, Subcommand};
use soldr_core::SoldrError;
use soldr_fetch::VersionSpec;

#[derive(Parser)]
#[command(name = "soldr", version, about = "Instant tools. Instant builds.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show cache status and tool info
    Status,
    /// Clear caches
    Clean,
    /// Show or set configuration
    Config,
    /// Inspect the compilation cache
    Cache,
    /// Show version
    Version,
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[tokio::main]
async fn main() {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && is_rustc_path(&raw_args[1]) {
        eprintln!("soldr: RUSTC_WRAPPER mode (not yet implemented)");
        std::process::exit(1);
    }

    if let Err(e) = run().await {
        eprintln!("soldr: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), SoldrError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Status => {
            println!("soldr {}", soldr_core::version());
            let target = soldr_core::TargetTriple::detect()?;
            println!("target: {target}");
            println!("(status not yet implemented)");
        }
        Commands::Clean => {
            println!("(clean not yet implemented)");
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Cache => {
            println!("(cache not yet implemented)");
        }
        Commands::Version => {
            println!("soldr {}", soldr_core::version());
        }
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                std::process::exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

            eprintln!("soldr: fetching {crate_name}...");
            let result = soldr_fetch::fetch_tool(&crate_name, &version).await?;

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let status = std::process::Command::new(&result.binary_path)
                .args(tool_args)
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }
    }

    Ok(())
}

fn is_rustc_path(arg: &str) -> bool {
    arg == "rustc"
        || arg.ends_with("/rustc")
        || arg.ends_with("\\rustc")
        || arg.ends_with("rustc.exe")
}

fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
    }
}
