//! `soldr toolchain prepare` / `ensure` driver, and its reld linker fetch.
//!
//! Split out of `toolchain.rs` to stay under the per-file line ceiling
//! (soldr#3276): this module owns the "install + components + targets +
//! plugins + linker" pipeline shared by `prepare` and `ensure`, and the
//! reld-specific fetch it reports in the `--json` payload.

use crate::core::{SoldrError, SoldrPaths};

use crate::toolchain::{
    cargo_install_plugin, format_plugin_label, rustup_component_add, rustup_target_add,
    rustup_toolchain_install_with_profile,
};

/// Summary of what `prepare` actually did, used by
/// [`crate::toolchain_ensure`] to populate the JSON payload.
#[derive(Debug, Default)]
pub(crate) struct PrepareSummary {
    pub components_added: Vec<String>,
    pub targets_added: Vec<String>,
    pub plugins_installed: Vec<String>,
    pub linker: Option<LinkerSummary>,
}

/// Reported in the `soldr toolchain prepare` / `soldr toolchain ensure`
/// `--json` payload when the project selects the `reld` linker (soldr#3276
/// T4). `None` on [`PrepareSummary`] means no linker fetch was needed --
/// either the project did not select `reld`, or nothing declares a
/// `rust-toolchain.toml` channel at all.
#[derive(serde::Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LinkerSummary {
    pub choice: String,
    pub path: String,
    pub version: String,
}

/// Shared inner driver for `prepare` / `ensure`. Returns the rustup /
/// cargo exit code (0 on success) plus a [`PrepareSummary`] of what was
/// actually attempted (we report every declared component/target/plugin
/// rustup didn't error on; for now we cannot cheaply diff "already
/// installed" from "newly installed" without parsing rustup output,
/// which is fragile across versions).
pub(crate) fn run_prepare_inner(
    channel: &str,
    manifest: &crate::core::RustToolchainManifest,
) -> Result<(i32, PrepareSummary), SoldrError> {
    let mut summary = PrepareSummary::default();

    let install_code = rustup_toolchain_install_with_profile(channel, manifest.profile.as_deref())?;
    if install_code != 0 {
        return Ok((install_code, summary));
    }

    if let Some(components) = manifest.components.as_deref() {
        for component in components {
            let code = rustup_component_add(channel, component)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.components_added.push(component.clone());
        }
    }

    if let Some(targets) = manifest.targets.as_deref() {
        for target in targets {
            let code = rustup_target_add(channel, target)?;
            if code != 0 {
                return Ok((code, summary));
            }
            summary.targets_added.push(target.clone());
        }
    }

    if let Some(soldr_section) = manifest.soldr.as_ref() {
        if !soldr_section.plugins.is_empty() {
            for (name, spec) in &soldr_section.plugins {
                let code = cargo_install_plugin(name, spec)?;
                if code != 0 {
                    return Ok((code, summary));
                }
                summary
                    .plugins_installed
                    .push(format_plugin_label(name, spec));
            }
        }
    }

    summary.linker = fetch_selected_linker()?;

    Ok((0, summary))
}

/// Fetch a pinned `reld` when the project explicitly selects it, mirroring
/// the resolution `soldr cargo`'s front door and `soldr prepare` already
/// apply (`linker::resolve_project_choice_from_cwd`: env > cargo config >
/// Cargo.toml metadata > user config). Returns `None` -- no network call --
/// when reld is not selected.
///
/// `run_prepare_inner` is sync but is also called from the async
/// `toolchain ensure` path, so the fetch runs on a dedicated OS thread with
/// its own current-thread runtime (a nested `block_on` would panic).
fn fetch_selected_linker() -> Result<Option<LinkerSummary>, SoldrError> {
    let paths = SoldrPaths::new()?;
    let host_triple = crate::core::TargetTriple::host()?.triple();
    let selection = crate::linker::resolve_project_choice_from_cwd(Some(&host_triple), &paths)?;
    if !selection.needs_reld() {
        return Ok(None);
    }

    let fetch_paths = paths.clone();
    let fetched = std::thread::spawn(move || -> Result<std::path::PathBuf, SoldrError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                SoldrError::Other(format!("could not create reld fetch runtime: {error}"))
            })?;
        runtime.block_on(crate::fetch::ensure_reld(&fetch_paths))
    })
    .join()
    .map_err(|_| SoldrError::Other("reld fetch thread panicked".to_string()))?;
    let path = match fetched {
        Ok(path) => path,
        Err(error) if selection.is_explicit() => {
            return Err(crate::linker::reld_fetch_error(error))
        }
        Err(_) => return Ok(None),
    };
    Ok(Some(LinkerSummary {
        choice: "reld".to_string(),
        path: path.to_string_lossy().into_owned(),
        version: crate::fetch::MANAGED_RELD_VERSION.to_string(),
    }))
}
