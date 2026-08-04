//! Catalogue-backed GNU/Linux compiler and glibc sysroot bundles.
//!
//! The bundle is produced by `zackees/soldr-toolchain` from locked
//! conda-forge GCC/binutils/sysroot artifacts. It replaces managed Zig for
//! blessed `*-unknown-linux-gnu` preparation: consumers select a Rust triple,
//! while Soldr selects a versioned compiler + sysroot bundle and exposes its
//! target-prefixed tool paths.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub const GNU_LINUX_TOOLCHAIN_VERSION: &str = "gcc-13.3.0-glibc-2.17-1";
const GNU_LINUX_TOOLCHAIN: &str = "gnu-linux-toolchain";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GnuLinuxToolchainTarget {
    X86_64,
    Aarch64,
}

impl GnuLinuxToolchainTarget {
    pub fn for_triple(triple: &str) -> Option<Self> {
        match triple {
            "x86_64-unknown-linux-gnu" => Some(Self::X86_64),
            "aarch64-unknown-linux-gnu" => Some(Self::Aarch64),
            _ => None,
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            Self::X86_64 => "linux-x64-gnu",
            Self::Aarch64 => "linux-arm64-gnu",
        }
    }

    pub const fn compiler_prefix(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-conda-linux-gnu",
            Self::Aarch64 => "aarch64-conda-linux-gnu",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GnuLinuxToolchain {
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub sysroot: PathBuf,
    pub target: GnuLinuxToolchainTarget,
}

impl GnuLinuxToolchain {
    pub fn tool_path(&self, tool: &str) -> PathBuf {
        self.bin_dir
            .join(format!("{}-{tool}", self.target.compiler_prefix()))
    }
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(GNU_LINUX_TOOLCHAIN, version, slug)
}

pub async fn ensure(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<GnuLinuxToolchain, SoldrError> {
    let target = GnuLinuxToolchainTarget::for_triple(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "catalogue-backed GNU/Linux toolchain does not support `{target_triple}`"
        ))
    })?;
    let root = super::syslib_common::ensure_syslib_bundle(
        paths,
        GNU_LINUX_TOOLCHAIN,
        GNU_LINUX_TOOLCHAIN_VERSION,
        target.slug(),
    )
    .await?;
    let bin_dir = root.join("bin");
    let sysroot = root.join(target.compiler_prefix()).join("sysroot");
    let toolchain = GnuLinuxToolchain {
        root,
        bin_dir,
        sysroot,
        target,
    };
    validate(&toolchain)?;
    Ok(toolchain)
}

fn validate(toolchain: &GnuLinuxToolchain) -> Result<(), SoldrError> {
    let missing: Vec<_> = ["gcc", "g++", "ar", "ranlib", "ld", "readelf"]
        .into_iter()
        .map(|tool| toolchain.tool_path(tool))
        .filter(|path| !path.is_file())
        .collect();
    if !missing.is_empty() {
        return Err(SoldrError::Archive(format!(
            "GNU/Linux toolchain {} is incomplete; missing {}",
            toolchain.root.display(),
            missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for child in ["usr/include", "usr/lib"] {
        let path = toolchain.sysroot.join(child);
        if !path.is_dir() {
            return Err(SoldrError::Archive(format!(
                "GNU/Linux toolchain {} is missing sysroot directory {}",
                toolchain.root.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn env_for_target(toolchain: &GnuLinuxToolchain, target_triple: &str) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_ascii_uppercase();
    let tool = |name| toolchain.tool_path(name).to_string_lossy().into_owned();
    let sysroot = toolchain.sysroot.to_string_lossy().into_owned();
    let pkg_config_libdir = format!("{sysroot}/usr/lib/pkgconfig:{sysroot}/usr/share/pkgconfig");
    vec![
        (
            "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT".to_string(),
            toolchain.root.to_string_lossy().into_owned(),
        ),
        ("SOLDR_GNU_LINUX_SYSROOT".to_string(), sysroot.clone()),
        ("CMAKE_SYSROOT".to_string(), sysroot.clone()),
        ("PKG_CONFIG_SYSROOT_DIR".to_string(), sysroot.clone()),
        ("PKG_CONFIG_LIBDIR".to_string(), pkg_config_libdir),
        (format!("CC_{target_u}"), tool("gcc")),
        (format!("CXX_{target_u}"), tool("g++")),
        (format!("AR_{target_u}"), tool("ar")),
        (format!("RANLIB_{target_u}"), tool("ranlib")),
        (format!("CARGO_TARGET_{target_u_upper}_LINKER"), tool("gcc")),
        (format!("CFLAGS_{target_u}"), format!("--sysroot={sysroot}")),
        (
            format!("CXXFLAGS_{target_u}"),
            format!("--sysroot={sysroot}"),
        ),
    ]
}

pub fn path_prefix(toolchain: &GnuLinuxToolchain) -> &Path {
    &toolchain.bin_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(maps_only_supported_gnu_targets, {
        assert_eq!(
            GnuLinuxToolchainTarget::for_triple("x86_64-unknown-linux-gnu"),
            Some(GnuLinuxToolchainTarget::X86_64)
        );
        assert_eq!(
            GnuLinuxToolchainTarget::for_triple("aarch64-unknown-linux-gnu"),
            Some(GnuLinuxToolchainTarget::Aarch64)
        );
        assert_eq!(
            GnuLinuxToolchainTarget::for_triple("x86_64-unknown-linux-musl"),
            None
        );
    });

    crate::timed_test!(asset_url_uses_catalogue_identity, {
        let url = asset_url_for(GNU_LINUX_TOOLCHAIN_VERSION, "linux-arm64-gnu");
        assert!(url.contains("/gnu-linux-toolchain/gcc-13.3.0-glibc-2.17-1/linux-arm64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(target_env_uses_prefixed_gcc_and_sysroot, {
        let toolchain = GnuLinuxToolchain {
            root: PathBuf::from("/managed"),
            bin_dir: PathBuf::from("/managed/bin"),
            sysroot: PathBuf::from("/managed/aarch64-conda-linux-gnu/sysroot"),
            target: GnuLinuxToolchainTarget::Aarch64,
        };
        let env = env_for_target(&toolchain, "aarch64-unknown-linux-gnu");
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_aarch64_unknown_linux_gnu").ends_with("aarch64-conda-linux-gnu-gcc"));
        assert!(lookup("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER")
            .ends_with("aarch64-conda-linux-gnu-gcc"));
        assert!(lookup("CFLAGS_aarch64_unknown_linux_gnu").contains("--sysroot="));
        assert!(lookup("PKG_CONFIG_SYSROOT_DIR").ends_with("/sysroot"));
        assert!(lookup("PKG_CONFIG_LIBDIR").contains("/usr/lib/pkgconfig"));
    });
}
