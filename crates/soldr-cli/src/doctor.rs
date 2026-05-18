//! `soldr doctor` — drift detector for `rust-toolchain.toml`. Extracted
//! from `main.rs` as part of issue #339.

use crate::cache::print_json;
use crate::{apply_implicit_toolchain_homes, rustup_binary, JSON_SCHEMA_VERSION};
use serde::Serialize;
use soldr_core::{suppress_windows_console_window, SoldrError};

#[derive(Serialize)]
struct DoctorComponent {
    name: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorTarget {
    triple: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorToolchain {
    channel: String,
    installed: bool,
}

#[derive(Serialize)]
struct DoctorOutput {
    schema_version: u32,
    command: &'static str,
    /// Absolute path to the inspected `rust-toolchain.toml`. `None`
    /// when no manifest exists in the current working directory.
    manifest_path: Option<String>,
    /// `None` when the manifest is missing or omits `channel`.
    toolchain: Option<DoctorToolchain>,
    components: Vec<DoctorComponent>,
    targets: Vec<DoctorTarget>,
    /// Whether any declared component or target is missing from the
    /// installed rustup state. Always `false` when no manifest exists.
    drift: bool,
    missing_components: Vec<String>,
    missing_targets: Vec<String>,
}

/// Implementation of `soldr doctor`. Read-only — never invokes
/// `rustup component add` / `target add` / `toolchain install`.
pub(crate) fn run_doctor(json: bool) -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest_path = workspace_root.join("rust-toolchain.toml");
    let manifest = soldr_core::read_rust_toolchain_manifest(&workspace_root)?;
    let manifest_present = manifest_path.exists();

    let Some(channel) = manifest.channel.as_deref() else {
        if json {
            let output = DoctorOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "doctor",
                manifest_path: manifest_present.then(|| manifest_path.display().to_string()),
                toolchain: None,
                components: Vec::new(),
                targets: Vec::new(),
                drift: false,
                missing_components: Vec::new(),
                missing_targets: Vec::new(),
            };
            print_json(&output)?;
        } else if manifest_present {
            println!(
                "manifest: {} (present but no [toolchain] channel declared)",
                manifest_path.display()
            );
            println!("result: no manifest fields to compare; nothing to do");
        } else {
            println!(
                "no rust-toolchain.toml found in {}",
                workspace_root.display()
            );
            println!("result: no manifest found; nothing to compare");
        }
        return Ok(0);
    };

    let toolchain_installed = rustup_toolchain_is_installed(channel)?;

    let declared_components: Vec<String> = manifest.components.clone().unwrap_or_default();
    let declared_targets: Vec<String> = manifest.targets.clone().unwrap_or_default();

    let installed_components = if toolchain_installed && !declared_components.is_empty() {
        rustup_installed_components(channel)?
    } else {
        Vec::new()
    };
    let installed_targets = if toolchain_installed && !declared_targets.is_empty() {
        rustup_installed_targets(channel)?
    } else {
        Vec::new()
    };

    let component_rows: Vec<DoctorComponent> = declared_components
        .iter()
        .map(|declared| DoctorComponent {
            name: declared.clone(),
            installed: component_is_installed(declared, &installed_components),
        })
        .collect();
    let target_rows: Vec<DoctorTarget> = declared_targets
        .iter()
        .map(|declared| DoctorTarget {
            triple: declared.clone(),
            installed: target_is_installed(declared, &installed_targets),
        })
        .collect();

    let missing_components: Vec<String> = component_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.name.clone())
        .collect();
    let missing_targets: Vec<String> = target_rows
        .iter()
        .filter(|row| !row.installed)
        .map(|row| row.triple.clone())
        .collect();

    let drift =
        !toolchain_installed || !missing_components.is_empty() || !missing_targets.is_empty();

    if json {
        let output = DoctorOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "doctor",
            manifest_path: Some(manifest_path.display().to_string()),
            toolchain: Some(DoctorToolchain {
                channel: channel.to_string(),
                installed: toolchain_installed,
            }),
            components: component_rows,
            targets: target_rows,
            drift,
            missing_components,
            missing_targets,
        };
        print_json(&output)?;
    } else {
        print_doctor_human(
            &manifest_path,
            channel,
            toolchain_installed,
            &component_rows,
            &target_rows,
            &missing_components,
            &missing_targets,
            drift,
        );
    }

    Ok(if drift { 1 } else { 0 })
}

fn component_is_installed(declared: &str, installed: &[String]) -> bool {
    let prefix = format!("{declared}-");
    installed
        .iter()
        .any(|entry| entry == declared || entry.starts_with(&prefix))
}

fn target_is_installed(declared: &str, installed: &[String]) -> bool {
    installed.iter().any(|entry| entry == declared)
}

#[allow(clippy::too_many_arguments)]
fn print_doctor_human(
    manifest_path: &std::path::Path,
    channel: &str,
    toolchain_installed: bool,
    components: &[DoctorComponent],
    targets: &[DoctorTarget],
    missing_components: &[String],
    missing_targets: &[String],
    drift: bool,
) {
    println!("manifest: {}", manifest_path.display());
    println!("toolchain: {channel}");
    println!(
        "  status: {}",
        if toolchain_installed {
            "installed"
        } else {
            "MISSING"
        }
    );

    if !components.is_empty() {
        println!();
        println!("components (declared {}):", components.len());
        let width = components
            .iter()
            .map(|row| row.name.len())
            .max()
            .unwrap_or(0);
        for row in components {
            println!(
                "  {:<width$}   {}",
                row.name,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    if !targets.is_empty() {
        println!();
        println!("targets (declared {}):", targets.len());
        let width = targets
            .iter()
            .map(|row| row.triple.len())
            .max()
            .unwrap_or(0);
        for row in targets {
            println!(
                "  {:<width$}   {}",
                row.triple,
                if row.installed {
                    "installed"
                } else {
                    "MISSING"
                },
                width = width
            );
        }
    }

    println!();
    if drift {
        let missing_component_count = missing_components.len();
        let missing_target_count = missing_targets.len();
        let mut parts: Vec<String> = Vec::new();
        if !toolchain_installed {
            parts.push("toolchain not installed".to_string());
        }
        if missing_component_count > 0 {
            parts.push(format!(
                "{missing_component_count} missing component{}",
                if missing_component_count == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if missing_target_count > 0 {
            parts.push(format!(
                "{missing_target_count} missing target{}",
                if missing_target_count == 1 { "" } else { "s" }
            ));
        }
        println!("result: drift detected ({})", parts.join(", "));
        println!(
            "hint: run `soldr toolchain prepare` to bring installed state in sync with manifest"
        );
    } else {
        println!("result: no drift");
    }
}

fn rustup_toolchain_is_installed(channel: &str) -> Result<bool, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["toolchain", "list"]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup toolchain list` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == channel
            || trimmed.starts_with(&format!("{channel} "))
            || trimmed.starts_with(&format!("{channel}-"))
    }))
}

fn rustup_installed_components(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["component", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup component list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn rustup_installed_targets(channel: &str) -> Result<Vec<String>, SoldrError> {
    let mut command = std::process::Command::new(rustup_binary());
    command.args(["target", "list", "--installed", "--toolchain", channel]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoldrError::Other(format!(
            "`rustup target list --installed --toolchain {channel}` failed with exit code {}: {stderr}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(parse_rustup_list_output(&output.stdout))
}

fn parse_rustup_list_output(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}
