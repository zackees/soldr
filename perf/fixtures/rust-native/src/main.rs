use anyhow::Result;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(version, about = "Rust plus native C benchmark fixture")]
struct Args {
    #[arg(long, default_value = "alpha,beta,gamma,delta")]
    values: String,

    #[arg(long, value_enum, default_value_t = OutputMode::Json)]
    mode: OutputMode,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputMode {
    Json,
    Text,
}

#[derive(Debug, Error)]
enum FixtureError {
    #[error("no values were provided")]
    Empty,
}

#[derive(Debug, Serialize, Deserialize)]
struct Summary {
    count: usize,
    checksum: u64,
    values: BTreeMap<String, usize>,
}

extern "C" {
    fn rust_native_checksum(bytes: *const u8, len: usize) -> u64;
}

fn summarize(input: &str) -> Result<Summary> {
    let mut values = BTreeMap::new();
    for value in input.split(',').map(str::trim).filter(|value| !value.is_empty()) {
        *values.entry(value.to_owned()).or_insert(0) += 1;
    }
    if values.is_empty() {
        return Err(FixtureError::Empty.into());
    }
    let checksum = unsafe { rust_native_checksum(input.as_ptr(), input.len()) };
    Ok(Summary {
        count: values.values().sum(),
        checksum,
        values,
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    let summary = summarize(&args.values)?;
    match args.mode {
        OutputMode::Json => println!("{}", serde_json::to_string(&summary)?),
        OutputMode::Text => println!("{summary:?}"),
    }
    Ok(())
}
