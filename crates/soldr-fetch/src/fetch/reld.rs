//! Pinned on-demand bootstrap for the `reld` linker (soldr#3276).
//!
//! `reld` is a host executable used by Cargo's linker invocation. Explicit
//! selection must therefore resolve one verified binary before Cargo starts,
//! rather than relying on ambient `PATH` state.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths, TargetTriple};
use crate::platform::host::facts::{HostArch, HostOs};

/// The release soldr installs when a project explicitly selects `reld`.
pub const MANAGED_RELD_VERSION: &str = "0.1.0";

/// Explicit local development override. It must name an absolute executable.
pub const RELD_BIN_ENV_VAR: &str = "SOLDR_RELD_BIN";

struct ReleaseAsset {
    triple: &'static str,
    extension: &'static str,
    sha256: &'static str,
}

/// Resolve a verified host executable for an explicit `reld` selection.
pub async fn ensure_reld(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(path) = reld_bin_from_env()? {
        return Ok(path);
    }

    let host = TargetTriple::host()?;
    let asset = release_asset_for(
        crate::platform::host::facts::os(),
        crate::platform::host::facts::arch(),
    )?;
    let binary = managed_binary_path(paths);
    if binary.is_file() {
        return Ok(binary);
    }

    let asset_name = format!(
        "reld-v{MANAGED_RELD_VERSION}-{}.{}",
        asset.triple, asset.extension
    );
    let url = format!(
        "https://github.com/zackees/reld/releases/download/v{MANAGED_RELD_VERSION}/{asset_name}"
    );
    eprintln!("soldr: fetching reld v{MANAGED_RELD_VERSION} ({asset_name})...");
    let resolved = super::archive::download_and_extract_with_pin(
        paths,
        "reld",
        MANAGED_RELD_VERSION,
        &url,
        &host,
        &["reld"],
        Some((&asset_name, asset.sha256)),
    )
    .await?;
    if !resolved.is_file() {
        return Err(SoldrError::Archive(format!(
            "reld archive did not produce an executable at {}",
            resolved.display()
        )));
    }
    eprintln!("soldr: downloaded reld v{MANAGED_RELD_VERSION}");
    Ok(resolved)
}

fn reld_bin_from_env() -> Result<Option<PathBuf>, SoldrError> {
    let Some(value) = std::env::var_os(RELD_BIN_ENV_VAR) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(SoldrError::Other(format!(
            "{RELD_BIN_ENV_VAR} must be an absolute path, got {}",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(SoldrError::Other(format!(
            "{RELD_BIN_ENV_VAR} does not name a file: {}",
            path.display()
        )));
    }
    Ok(Some(path))
}

fn managed_binary_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .bin
        .join(format!("reld-{MANAGED_RELD_VERSION}"))
        .join(format!("reld{}", std::env::consts::EXE_SUFFIX))
}

fn release_asset_for(os: HostOs, arch: HostArch) -> Result<ReleaseAsset, SoldrError> {
    let asset = match (os, arch) {
        // Prefer the static musl release on Linux x86_64 so the bootstrap does
        // not depend on a runner's glibc version.
        (HostOs::Linux, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-unknown-linux-musl",
            extension: "tar.gz",
            sha256: "f22603473c72e97272793e19d8c40bffd2002215be4c034436b27a9bc1c7e660",
        },
        (HostOs::Linux, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-unknown-linux-gnu",
            extension: "tar.gz",
            sha256: "3ef6633abfd2cab02ff23e223acbdc2d254d6576a864456a8ca54832691bf3b0",
        },
        (HostOs::MacOs, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-apple-darwin",
            extension: "tar.gz",
            sha256: "b92a2c2dca417f8057f000bce71aadd17ce6434b1659af29a6f698db76159cb2",
        },
        (HostOs::MacOs, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-apple-darwin",
            extension: "tar.gz",
            sha256: "b726a0323c5690aa2ee9f94893fbfc086d4e534c7fef9a7f6eb31bd3799ffb67",
        },
        (HostOs::Windows, HostArch::X86_64) => ReleaseAsset {
            triple: "x86_64-pc-windows-msvc",
            extension: "zip",
            sha256: "2ba7fe09af8c66d0d020d8ee7d520ca1c0f5d449b1c98b23c82d170a92104d06",
        },
        (HostOs::Windows, HostArch::Aarch64) => ReleaseAsset {
            triple: "aarch64-pc-windows-msvc",
            extension: "zip",
            sha256: "cfa1d9c0468a19f3178b70189ca25c26d5e412ee1602074ef94d4b462a42797f",
        },
        (os, arch) => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "reld v{MANAGED_RELD_VERSION} has no managed asset for host {arch:?} on {os:?}"
            )));
        }
    };
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_assets_cover_supported_hosts_and_prefer_static_linux_x64() {
        let linux = release_asset_for(HostOs::Linux, HostArch::X86_64).expect("linux x64");
        assert_eq!(linux.triple, "x86_64-unknown-linux-musl");
        assert_eq!(linux.extension, "tar.gz");
        assert_eq!(linux.sha256.len(), 64);

        let windows = release_asset_for(HostOs::Windows, HostArch::Aarch64).expect("windows arm");
        assert_eq!(windows.triple, "aarch64-pc-windows-msvc");
        assert_eq!(windows.extension, "zip");

        let mac = release_asset_for(HostOs::MacOs, HostArch::Aarch64).expect("mac arm");
        assert_eq!(mac.triple, "aarch64-apple-darwin");
    }

    #[test]
    fn unknown_host_is_rejected() {
        assert!(release_asset_for(HostOs::Linux, HostArch::Unknown("riscv64")).is_err());
    }
}
