//! `soldr doctor` — drift detector for `rust-toolchain.toml`. Extracted
//! from `main.rs` as part of issue #339.

use crate::cache::print_json;
use crate::{apply_implicit_toolchain_homes, rustup_binary, JSON_SCHEMA_VERSION};
use serde::Serialize;
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use soldr_fetch::{ZccacheBinarySummary, ZccacheSource};

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
    /// Managed zccache resolution: where the binaries live, whether
    /// the `SOLDR_ZCCACHE_LOCAL_DIR` override is active, and where to
    /// point a debugger for symbol resolution.
    managed_zccache: DoctorManagedZccache,
}

#[derive(Serialize)]
struct DoctorManagedZccache {
    /// `managed`, `local`, or `none` (nothing fetched yet).
    source: &'static str,
    /// Version label. Empty when source is `none`.
    version: String,
    /// Directory whose binaries are actually executed.
    runtime_dir: String,
    /// For local builds, the path the user set in
    /// `SOLDR_ZCCACHE_LOCAL_DIR`. Null for managed builds.
    source_dir: Option<String>,
    /// Absolute path to the active CLI binary, if present.
    cli_path: Option<String>,
    /// Absolute path to the active daemon binary, if present.
    daemon_path: Option<String>,
    /// Absolute path to the active fingerprint binary, if present.
    fp_path: Option<String>,
    /// Number of debug-info sidecars present (PDBs on Windows, DWPs
    /// on Linux, dSYMs on macOS).
    debug_info_found: usize,
    /// Number of binaries we expected debug-info for (always 3).
    debug_info_expected: usize,
    /// Path to pass to `cdb -y` / `_NT_SYMBOL_PATH` when attaching.
    symbol_path: String,
}

impl DoctorManagedZccache {
    fn from_summary(summary: &ZccacheBinarySummary) -> Self {
        Self {
            source: summary.source.as_str(),
            version: summary.version.clone(),
            runtime_dir: summary.runtime_dir.display().to_string(),
            source_dir: summary.source_dir.as_ref().map(|p| p.display().to_string()),
            cli_path: summary.cli_path.as_ref().map(|p| p.display().to_string()),
            daemon_path: summary
                .daemon_path
                .as_ref()
                .map(|p| p.display().to_string()),
            fp_path: summary.fp_path.as_ref().map(|p| p.display().to_string()),
            debug_info_found: summary.debug_info_found,
            debug_info_expected: summary.debug_info_expected,
            symbol_path: summary.symbol_path.display().to_string(),
        }
    }
}

/// Implementation of `soldr doctor`. Read-only — never invokes
/// `rustup component add` / `target add` / `toolchain install`.
pub(crate) fn run_doctor(json: bool) -> Result<i32, SoldrError> {
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest_path = workspace_root.join("rust-toolchain.toml");
    let manifest = soldr_core::read_rust_toolchain_manifest(&workspace_root)?;
    let manifest_present = manifest_path.exists();
    let zccache_summary = collect_zccache_summary()?;

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
                managed_zccache: DoctorManagedZccache::from_summary(&zccache_summary),
            };
            print_json(&output)?;
        } else if manifest_present {
            println!(
                "manifest: {} (present but no [toolchain] channel declared)",
                manifest_path.display()
            );
            print_managed_zccache_human(&zccache_summary);
            println!("result: no manifest fields to compare; nothing to do");
        } else {
            println!(
                "no rust-toolchain.toml found in {}",
                workspace_root.display()
            );
            print_managed_zccache_human(&zccache_summary);
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
            managed_zccache: DoctorManagedZccache::from_summary(&zccache_summary),
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
            &zccache_summary,
        );
    }

    Ok(if drift { 1 } else { 0 })
}

/// Collect zccache binary resolution info for doctor output. Read-only:
/// honors `SOLDR_ZCCACHE_LOCAL_DIR` but doesn't trigger a managed
/// fetch.
fn collect_zccache_summary() -> Result<ZccacheBinarySummary, SoldrError> {
    let paths = SoldrPaths::new()?;
    soldr_fetch::zccache_binary_summary(&paths)
}

fn print_managed_zccache_human(summary: &ZccacheBinarySummary) {
    println!();
    println!("managed zccache:");
    match summary.source {
        ZccacheSource::Local => {
            println!(
                "  source:        local ({})",
                soldr_fetch::ZCCACHE_LOCAL_DIR_ENV_VAR
            );
            if let Some(dir) = &summary.source_dir {
                println!("  source dir:    {}", dir.display());
            }
            if !summary.version.is_empty() {
                println!("  version:       {}", summary.version);
            }
        }
        ZccacheSource::Managed => {
            println!(
                "  source:        managed ({})",
                soldr_fetch::MANAGED_ZCCACHE_VERSION
            );
        }
        ZccacheSource::None => {
            println!(
                "  source:        managed ({}, not fetched yet)",
                soldr_fetch::MANAGED_ZCCACHE_VERSION
            );
        }
    }
    println!("  runtime dir:   {}", summary.runtime_dir.display());
    match &summary.cli_path {
        Some(p) => println!("  active cli:    {}", p.display()),
        None => println!("  active cli:    <not present>"),
    }
    match &summary.daemon_path {
        Some(p) => println!("  active daemon: {}", p.display()),
        None => println!("  active daemon: <not present>"),
    }
    match &summary.fp_path {
        Some(p) => println!("  active fp:     {}", p.display()),
        None => println!("  active fp:     <not present>"),
    }
    let pdb_hint = if summary.debug_info_found == 0 {
        "no PDBs present; build zccache with `[profile.release] debug = \"line-tables-only\"` to get them"
    } else if summary.debug_info_found < summary.debug_info_expected {
        "partial — some sidecars missing"
    } else {
        "complete"
    };
    println!(
        "  pdbs found:    {}/{} ({})",
        summary.debug_info_found, summary.debug_info_expected, pdb_hint
    );
    println!("  symbol path:   {}", summary.symbol_path.display());
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
    zccache_summary: &ZccacheBinarySummary,
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

    print_managed_zccache_human(zccache_summary);

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
