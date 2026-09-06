//! Filesystem readiness for the Dylint nightly toolchain.
//!
//! Kept separate from channel selection because this is a durable safety
//! boundary: a directory left by an interrupted manager operation must never
//! be accepted as a usable Dylint toolchain.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, TargetTriple};
use crate::toolchain_readiness::{classify_toolchain_dir, ToolchainReadiness};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DylintToolchainReadiness {
    Missing,
    Ready {
        qualified: String,
        directory: PathBuf,
    },
    Partial {
        qualified: String,
        directory: PathBuf,
        missing: Vec<&'static str>,
    },
}

/// Filesystem-only readiness probe for the Dylint nightly. A directory alone
/// is not evidence of a usable toolchain: both the channel manifest and the
/// compiler binary must be present.
pub(crate) fn dylint_toolchain_readiness_at(
    manager_home: &Path,
    channel: &str,
) -> DylintToolchainReadiness {
    let toolchains_dir = manager_home.join("toolchains");
    let mut candidates = Vec::new();
    let exact = toolchains_dir.join(channel);
    if exact.is_dir() {
        candidates.push(exact);
    }
    if let Ok(triple) = TargetTriple::host() {
        let host = toolchains_dir.join(format!("{channel}-{}", triple.triple()));
        if host.is_dir() && !candidates.contains(&host) {
            candidates.push(host);
        }
    }
    let Ok(entries) = std::fs::read_dir(&toolchains_dir) else {
        return DylintToolchainReadiness::Missing;
    };
    let prefix = format!("{channel}-");
    for entry in entries.filter_map(Result::ok) {
        let directory = entry.path();
        if entry.file_name().to_string_lossy().starts_with(&prefix)
            && directory.is_dir()
            && !candidates.contains(&directory)
        {
            candidates.push(directory);
        }
    }
    let Some(directory) = candidates.into_iter().next() else {
        return DylintToolchainReadiness::Missing;
    };
    let qualified = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(channel)
        .to_string();
    match classify_toolchain_dir(&directory) {
        ToolchainReadiness::Ready => DylintToolchainReadiness::Ready {
            qualified,
            directory,
        },
        ToolchainReadiness::Partial(missing) => DylintToolchainReadiness::Partial {
            qualified,
            directory,
            missing: missing.paths(),
        },
        // The candidate was selected only after `is_dir`; a deletion race is
        // indistinguishable from no installed selected toolchain and must not
        // be reclassified as ready.
        ToolchainReadiness::Missing => DylintToolchainReadiness::Missing,
    }
}

pub(crate) fn ensure_dylint_toolchain_ready_at<F>(
    manager_home: &Path,
    channel: &str,
    install: F,
) -> Result<(), SoldrError>
where
    F: FnOnce() -> Result<i32, SoldrError>,
{
    match dylint_toolchain_readiness_at(manager_home, channel) {
        DylintToolchainReadiness::Ready { .. } => Ok(()),
        DylintToolchainReadiness::Partial {
            qualified,
            directory,
            missing,
        } => Err(partial_dylint_toolchain_error(
            &qualified, &directory, &missing,
        )),
        DylintToolchainReadiness::Missing => {
            let code = install()?;
            if code != 0 {
                return Err(SoldrError::Other(format!(
                    "manager failed to install {channel} (exit {code})"
                )));
            }
            match dylint_toolchain_readiness_at(manager_home, channel) {
                DylintToolchainReadiness::Ready { .. } => Ok(()),
                DylintToolchainReadiness::Partial {
                    qualified,
                    directory,
                    missing,
                } => Err(partial_dylint_toolchain_error(&qualified, &directory, &missing)),
                DylintToolchainReadiness::Missing => Err(SoldrError::Other(format!(
                    "manager reported successful installation of {channel}, but no toolchain directory was created"
                ))),
            }
        }
    }
}

pub(crate) fn partial_dylint_toolchain_error(
    qualified: &str,
    directory: &Path,
    missing: &[&str],
) -> SoldrError {
    let manager = ["rust", "up"].concat();
    SoldrError::Other(format!(
        "Dylint toolchain {qualified} is partial at {}: missing {}. Do not add components to this incomplete toolchain. Run `soldr {manager} toolchain uninstall {qualified}` then rerun `soldr dylint`. If the manager cannot remove it, remove the exact directory `{}` and rerun `soldr dylint`.",
        directory.display(),
        missing.join(", "),
        directory.display(),
    ))
}
