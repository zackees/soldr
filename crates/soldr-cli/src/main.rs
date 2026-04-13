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
    /// Run Cargo through soldr's front door
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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
        std::process::exit(run_rustc_wrapper(&raw_args).unwrap_or_else(report_and_exit));
    }

    if let Err(e) = run().await {
        eprintln!("soldr: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), SoldrError> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Cargo { args } => {
            std::process::exit(run_cargo_front_door(&args)?);
        }
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

fn report_and_exit(error: SoldrError) -> i32 {
    eprintln!("soldr: {error}");
    1
}

fn is_rustc_path(arg: &str) -> bool {
    arg == "rustc"
        || arg.ends_with("/rustc")
        || arg.ends_with("\\rustc")
        || arg.ends_with("rustc.exe")
}

fn run_rustc_wrapper(raw_args: &[String]) -> Result<i32, SoldrError> {
    let rustc = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing rustc path in wrapper mode".into()))?;
    let rustc = if rustc == "rustc" {
        resolve_toolchain_binary("rustc")?
    } else {
        rustc.into()
    };

    let status = std::process::Command::new(rustc)
        .args(&raw_args[2..])
        .status()?;

    Ok(status.code().unwrap_or(1))
}

fn run_cargo_front_door(args: &[String]) -> Result<i32, SoldrError> {
    let cargo = resolve_toolchain_binary("cargo")?;
    let rustc = resolve_toolchain_binary("rustc")?;
    let current_exe = std::env::current_exe()?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");

    let mut command = std::process::Command::new(cargo);
    command.args(args);
    command.env("RUSTC_WRAPPER", current_exe);
    command.env("RUSTC", rustc);
    command.env("PATH", prepend_path(&cargo_bin_dir, existing_path.as_deref())?);

    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn prepend_path(
    dir: &std::path::Path,
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing_path) = existing_path {
        paths.extend(std::env::split_paths(existing_path));
    }
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}

fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    let output = std::process::Command::new("rustup")
        .args(["which", tool])
        .output()?;

    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "failed to resolve {tool} via rustup: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(SoldrError::Other(format!(
            "rustup did not return a path for {tool}"
        )));
    }

    Ok(path.into())
}

fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
    }
}
