//! Driver for `soldr bootstrap`. Thin wrapper over
//! [`crate::fetch::bootstrap_rustup`] that prints a user-facing summary or the
//! stable machine-facing JSON form.

use crate::core::{SoldrError, SoldrPaths};
use serde::Serialize;

#[derive(Serialize)]
struct BootstrapJson {
    schema_version: u32,
    rustup_path: String,
    already_installed: bool,
    source_url: Option<String>,
    managed_cargo_home: String,
    managed_rustup_home: String,
}

const SCHEMA_VERSION: u32 = 1;

pub(crate) async fn run_bootstrap(json: bool) -> Result<i32, SoldrError> {
    let paths = SoldrPaths::new()?;
    let report = crate::fetch::bootstrap_rustup(&paths).await?;

    let cargo_home = crate::fetch::managed_cargo_home(&paths);
    let rustup_home = crate::fetch::managed_rustup_home(&paths);

    if json {
        let payload = BootstrapJson {
            schema_version: SCHEMA_VERSION,
            rustup_path: report.rustup_path.display().to_string(),
            already_installed: report.already_installed,
            source_url: report.source_url.clone(),
            managed_cargo_home: cargo_home.display().to_string(),
            managed_rustup_home: rustup_home.display().to_string(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).map_err(|e| SoldrError::Other(e.to_string()))?
        );
        return Ok(0);
    }

    if report.already_installed {
        println!(
            "soldr bootstrap: rustup already installed at {}",
            report.rustup_path.display()
        );
    } else {
        println!(
            "soldr bootstrap: installed rustup at {} (from {})",
            report.rustup_path.display(),
            report.source_url.as_deref().unwrap_or("<unknown>")
        );
        println!(
            "soldr bootstrap: managed CARGO_HOME  = {}",
            cargo_home.display()
        );
        println!(
            "soldr bootstrap: managed RUSTUP_HOME = {}",
            rustup_home.display()
        );
        println!(
            "soldr bootstrap: next step: `soldr toolchain prepare` (or `soldr cargo build`) \
             — soldr will pick up the managed rustup automatically."
        );
    }

    Ok(0)
}
