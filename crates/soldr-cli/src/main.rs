use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "soldr", version, about = "Instant tools. Instant builds.")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch and run a crate binary
    Run {
        /// Crate name (e.g. maturin, cargo-dylint)
        crate_name: String,
        /// Arguments to pass to the tool
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Build the current project with compilation caching
    Build {
        /// Arguments to pass to cargo build
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Show cache status
    Status,
    /// Clear all caches (tools + compilation artifacts)
    Clean,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { crate_name, args } => {
            println!("soldr run {crate_name} {}", args.join(" "));
            println!("(not yet implemented)");
        }
        Commands::Build { args } => {
            println!("soldr build {}", args.join(" "));
            println!("(not yet implemented)");
        }
        Commands::Status => {
            println!("soldr {}", soldr_core::version());
            println!("(not yet implemented)");
        }
        Commands::Clean => {
            println!("soldr clean");
            println!("(not yet implemented)");
        }
    }
}
