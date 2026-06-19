// Trivial body — the bench measures *build* time, not runtime. Keep this
// minimal so any compile delta we observe is dominated by dep crates, not
// by code edits to this file.

use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(version, about = "bench-demo-small noop binary", long_about = None)]
struct Args {
    #[arg(long, default_value = "hello")]
    message: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Greeting {
    message: String,
    count: u32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let g = Greeting { message: args.message, count: 1 };
    println!("{}", serde_json::to_string(&g)?);
    Ok(())
}
