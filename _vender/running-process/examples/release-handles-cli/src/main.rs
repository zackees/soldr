use std::env;
use std::error::Error;
use std::path::PathBuf;

use running_process::maintenance::run_release_handles;

fn main() -> Result<(), Box<dyn Error>> {
    let path = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);

    let outcome = run_release_handles(&path)?;
    println!("{}", outcome.to_json());

    Ok(())
}
