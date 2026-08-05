//! Catalogue-backed musl/Linux compiler and static sysroot bundles.
//!
//! Canonical musl builds use these pinned `musl.cc`-derived bundles instead of
//! Zig.  The bundle contains target-prefixed GCC/binutils plus the complete
//! musl runtime and CRT startup objects, so every C consumer stays below the
//! managed root.

use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};

pub const MUSL_LINUX_TOOLCHAIN_VERSION: &str = "gcc-11.2.1-musl-20211123-1";
const MUSL_LINUX_TOOLCHAIN: &str = "musl-linux-toolchain";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuslLinuxToolchainTarget {
    X86_64,
    Aarch64,
}

impl MuslLinuxToolchainTarget {
    pub fn for_triple(triple: &str) -> Option<Self> {
        match triple {
            "x86_64-unknown-linux-musl" => Some(Self::X86_64),
            "aarch64-unknown-linux-musl" => Some(Self::Aarch64),
            _ => None,
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            Self::X86_64 => "linux-x64-musl",
            Self::Aarch64 => "linux-arm64-musl",
        }
    }

    pub const fn compiler_prefix(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-linux-musl",
            Self::Aarch64 => "aarch64-linux-musl",
        }
    }
}

pub fn env_keys_for_target(target_triple: &str) -> Vec<String> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_ascii_uppercase();
    vec![
        format!("CC_{target_u}"),
        format!("CXX_{target_u}"),
        format!("AR_{target_u}"),
        format!("RANLIB_{target_u}"),
        format!("CFLAGS_{target_u}"),
        format!("CXXFLAGS_{target_u}"),
        format!("CARGO_TARGET_{target_u_upper}_LINKER"),
        "CMAKE_SYSROOT".to_string(),
        "PKG_CONFIG_SYSROOT_DIR".to_string(),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_MUSL_LINUX_SYSROOT".to_string(),
        "SOLDR_MUSL_LINUX_TOOLCHAIN_ROOT".to_string(),
    ]
}

#[derive(Debug, Clone)]
pub struct MuslLinuxToolchain {
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub sysroot: PathBuf,
    pub target: MuslLinuxToolchainTarget,
}

impl MuslLinuxToolchain {
    pub fn tool_path(&self, tool: &str) -> PathBuf {
        self.bin_dir
            .join(format!("{}-{tool}", self.target.compiler_prefix()))
    }
}

pub fn asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MUSL_LINUX_TOOLCHAIN, version, slug)
}

pub async fn ensure(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<MuslLinuxToolchain, SoldrError> {
    let target = MuslLinuxToolchainTarget::for_triple(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "catalogue-backed musl/Linux toolchain does not support `{target_triple}`"
        ))
    })?;
    let root = super::syslib_common::ensure_syslib_bundle(
        paths,
        MUSL_LINUX_TOOLCHAIN,
        MUSL_LINUX_TOOLCHAIN_VERSION,
        target.slug(),
    )
    .await?;
    let toolchain = MuslLinuxToolchain {
        bin_dir: root.join("bin"),
        sysroot: root.join(target.compiler_prefix()),
        root,
        target,
    };
    validate(&toolchain)?;
    Ok(toolchain)
}

fn validate(toolchain: &MuslLinuxToolchain) -> Result<(), SoldrError> {
    let missing: Vec<_> = [
        "gcc", "g++", "ar", "ranlib", "ld", "readelf", "strip", "objcopy",
    ]
    .into_iter()
    .map(|tool| toolchain.tool_path(tool))
    .filter(|path| !path.is_file())
    .collect();
    if !missing.is_empty() {
        return Err(SoldrError::Archive(format!(
            "musl/Linux toolchain {} is incomplete; missing {}",
            toolchain.root.display(),
            missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for child in [
        "include",
        "lib",
        "lib/crt1.o",
        "lib/rcrt1.o",
        "lib/crti.o",
        "lib/crtn.o",
        "lib/libc.a",
        "lib/libstdc++.a",
    ] {
        let path = toolchain.sysroot.join(child);
        if !path.exists() {
            return Err(SoldrError::Archive(format!(
                "musl/Linux toolchain {} is missing required runtime path {}",
                toolchain.root.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn env_for_target(
    toolchain: &MuslLinuxToolchain,
    target_triple: &str,
) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_ascii_uppercase();
    let tool = |name| toolchain.tool_path(name).to_string_lossy().into_owned();
    let sysroot = toolchain.sysroot.to_string_lossy().into_owned();
    let pkg_config_libdir = format!("{sysroot}/lib/pkgconfig:{sysroot}/share/pkgconfig");
    vec![
        (
            "SOLDR_MUSL_LINUX_TOOLCHAIN_ROOT".to_string(),
            toolchain.root.to_string_lossy().into_owned(),
        ),
        ("SOLDR_MUSL_LINUX_SYSROOT".to_string(), sysroot.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(maps_only_supported_musl_targets, {
        assert_eq!(
            MuslLinuxToolchainTarget::for_triple("x86_64-unknown-linux-musl"),
            Some(MuslLinuxToolchainTarget::X86_64)
        );
        assert_eq!(
            MuslLinuxToolchainTarget::for_triple("aarch64-unknown-linux-musl"),
            Some(MuslLinuxToolchainTarget::Aarch64)
        );
        assert_eq!(
            MuslLinuxToolchainTarget::for_triple("x86_64-unknown-linux-gnu"),
            None
        );
    });

    crate::timed_test!(target_env_is_managed_and_sysrooted, {
        let toolchain = MuslLinuxToolchain {
            root: PathBuf::from("/managed"),
            bin_dir: PathBuf::from("/managed/bin"),
            sysroot: PathBuf::from("/managed/aarch64-linux-musl"),
            target: MuslLinuxToolchainTarget::Aarch64,
        };
        let env = env_for_target(&toolchain, "aarch64-unknown-linux-musl");
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_aarch64_unknown_linux_musl").ends_with("aarch64-linux-musl-gcc"));
        assert!(lookup("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER")
            .ends_with("aarch64-linux-musl-gcc"));
        assert!(lookup("CFLAGS_aarch64_unknown_linux_musl").contains("--sysroot="));
        for (key, _) in &env {
            assert!(env_keys_for_target("aarch64-unknown-linux-musl").contains(key));
        }
        assert!(
            asset_url_for(MUSL_LINUX_TOOLCHAIN_VERSION, "linux-arm64-musl")
                .contains("/musl-linux-toolchain/")
        );
    });

    crate::timed_test!(incomplete_bundle_fails_with_the_missing_managed_path, {
        let dir = tempfile::tempdir().expect("tempdir");
        let toolchain = MuslLinuxToolchain {
            root: dir.path().to_path_buf(),
            bin_dir: dir.path().join("bin"),
            sysroot: dir.path().join("aarch64-linux-musl"),
            target: MuslLinuxToolchainTarget::Aarch64,
        };
        let error = validate(&toolchain).expect_err("incomplete bundle must fail");
        let rendered = error.to_string();
        assert!(rendered.contains("musl/Linux toolchain"));
        assert!(rendered.contains("aarch64-linux-musl-gcc"));
    });
}
