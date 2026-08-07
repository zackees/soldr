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
pub const GNU_LINUX_GLIBC_BASELINE: &str = "2.17";
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

    pub const fn supports_glibc_floor(self) -> bool {
        true
    }
}

/// Whether a GNU Linux triple has a catalogue sysroot that enforces the
/// requested glibc ABI baseline.
pub fn supports_glibc_floor(triple: &str, floor: &str) -> bool {
    GnuLinuxToolchainTarget::for_triple(&triple.trim().to_ascii_lowercase())
        .is_some_and(|target| target.supports_glibc_floor())
        && floor == GNU_LINUX_GLIBC_BASELINE
}

/// The environment keys a GNU bundle applies for `target_triple`.
///
/// The target plan consumes this list as well, so the machine-readable
/// capability output cannot drift from the actual prepared environment.
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
        "CMAKE_C_COMPILER".to_string(),
        "CMAKE_CXX_COMPILER".to_string(),
        "CMAKE_AR".to_string(),
        "CMAKE_RANLIB".to_string(),
        "CMAKE_LINKER".to_string(),
        "CMAKE_SYSROOT".to_string(),
        "PKG_CONFIG_SYSROOT_DIR".to_string(),
        "PKG_CONFIG_LIBDIR".to_string(),
        "SOLDR_GNU_LINUX_SYSROOT".to_string(),
        "SOLDR_GNU_LINUX_TOOLCHAIN_ROOT".to_string(),
    ]
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
    let make_toolchain = |root: PathBuf| {
        let bin_dir = root.join("bin");
        let sysroot = root.join(target.compiler_prefix()).join("sysroot");
        GnuLinuxToolchain {
            root,
            bin_dir,
            sysroot,
            target,
        }
    };
    let ensure_bundle = || {
        super::syslib_common::ensure_syslib_bundle(
            paths,
            GNU_LINUX_TOOLCHAIN,
            GNU_LINUX_TOOLCHAIN_VERSION,
            target.slug(),
        )
    };
    let toolchain = make_toolchain(ensure_bundle().await?);
    if let Err(err) = validate(&toolchain) {
        if !sysroot_has_wrong_flavor_link(&toolchain) {
            return Err(err);
        }
        // soldr#2300 self-heal: an extraction by an older soldr on Windows
        // left file-flavor NTFS symlinks pointing at directories, which are
        // non-traversable. Re-extracting with the fixed symlink-aware
        // unpack (`fetch::tar_extract`) produces a working sysroot.
        let install_root = toolchain
            .root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| toolchain.root.clone());
        eprintln!(
            "soldr: GNU/Linux sysroot under {} contains non-traversable symlinks \
             (extracted by an earlier soldr on Windows); deleting and re-extracting...",
            install_root.display()
        );
        std::fs::remove_dir_all(&install_root)?;
        let toolchain = make_toolchain(ensure_bundle().await?);
        validate(&toolchain)?;
        return Ok(toolchain);
    }
    Ok(toolchain)
}

/// True when a required sysroot child exists on disk but does not pass a
/// link-following directory stat — the soldr#2300 signature of a
/// wrong-flavor NTFS symlink (a *file* symlink pointing at a directory)
/// created by tar extraction on Windows before the `tar_extract` fix.
fn sysroot_has_wrong_flavor_link(toolchain: &GnuLinuxToolchain) -> bool {
    SYSROOT_REQUIRED_DIRS.iter().any(|child| {
        let path = toolchain.sysroot.join(child);
        !path.is_dir() && path.symlink_metadata().is_ok()
    })
}

const SYSROOT_REQUIRED_DIRS: [&str; 2] = ["usr/include", "usr/lib"];

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
    for child in SYSROOT_REQUIRED_DIRS {
        let path = toolchain.sysroot.join(child);
        if path.is_dir() {
            continue;
        }
        // soldr#2300: distinguish "missing" from "present but not
        // traversable" (a wrong-flavor NTFS symlink from an older
        // extraction) and name the exact remedy.
        if path.symlink_metadata().is_ok() {
            let install_root = toolchain.root.parent().unwrap_or(toolchain.root.as_path());
            return Err(SoldrError::Archive(format!(
                "GNU/Linux toolchain {} sysroot entry {} exists but is not a \
                 traversable directory (likely a wrong-flavor symlink created \
                 by an older soldr extraction on Windows); delete {} and re-run \
                 so soldr re-extracts the bundle",
                toolchain.root.display(),
                path.display(),
                install_root.display()
            )));
        }
        return Err(SoldrError::Archive(format!(
            "GNU/Linux toolchain {} is missing sysroot directory {}",
            toolchain.root.display(),
            path.display()
        )));
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
        ("CMAKE_C_COMPILER".to_string(), tool("gcc")),
        ("CMAKE_CXX_COMPILER".to_string(), tool("g++")),
        ("CMAKE_AR".to_string(), tool("ar")),
        ("CMAKE_RANLIB".to_string(), tool("ranlib")),
        ("CMAKE_LINKER".to_string(), tool("ld")),
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

    /// A minimal on-disk toolchain layout that passes the tool-binary
    /// half of `validate()`. `usr/lib` is created per `lib_kind`:
    /// a real directory, a regular file (stand-in for a wrong-flavor
    /// symlink: exists, but fails a link-following dir stat), or absent.
    fn synth_toolchain(tmp: &Path, lib_kind: &str) -> GnuLinuxToolchain {
        let root = tmp.join("linux-x64-gnu").join("package");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        for tool in ["gcc", "g++", "ar", "ranlib", "ld", "readelf"] {
            std::fs::write(bin_dir.join(format!("x86_64-conda-linux-gnu-{tool}")), "").unwrap();
        }
        let sysroot = root.join("x86_64-conda-linux-gnu").join("sysroot");
        std::fs::create_dir_all(sysroot.join("usr/include")).unwrap();
        match lib_kind {
            "dir" => std::fs::create_dir_all(sysroot.join("usr/lib")).unwrap(),
            "wrong-flavor" => std::fs::write(sysroot.join("usr/lib"), "").unwrap(),
            "absent" => {}
            other => panic!("unknown lib_kind {other}"),
        }
        GnuLinuxToolchain {
            root,
            bin_dir,
            sysroot,
            target: GnuLinuxToolchainTarget::X86_64,
        }
    }

    crate::timed_test!(validate_accepts_real_sysroot_dirs, {
        let tmp = tempfile::tempdir().unwrap();
        let toolchain = synth_toolchain(tmp.path(), "dir");
        validate(&toolchain).expect("real dirs must validate");
        assert!(!sysroot_has_wrong_flavor_link(&toolchain));
    });

    crate::timed_test!(validate_names_remedy_for_non_traversable_sysroot_entry, {
        let tmp = tempfile::tempdir().unwrap();
        let toolchain = synth_toolchain(tmp.path(), "wrong-flavor");
        let err = validate(&toolchain).expect_err("wrong-flavor entry must fail");
        let message = err.to_string();
        assert!(
            message.contains("not a traversable directory"),
            "must not report the entry as merely missing: {message}"
        );
        let install_root = toolchain.root.parent().unwrap();
        assert!(
            message.contains(&install_root.display().to_string()),
            "must name the exact directory to delete: {message}"
        );
        assert!(sysroot_has_wrong_flavor_link(&toolchain));
    });

    crate::timed_test!(validate_reports_truly_missing_sysroot_dir, {
        let tmp = tempfile::tempdir().unwrap();
        let toolchain = synth_toolchain(tmp.path(), "absent");
        let err = validate(&toolchain).expect_err("absent entry must fail");
        let message = err.to_string();
        assert!(
            message.contains("missing sysroot directory"),
            "absent entry keeps the missing wording: {message}"
        );
        assert!(
            !sysroot_has_wrong_flavor_link(&toolchain),
            "absent entry must NOT trigger the self-heal re-extract"
        );
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
        assert!(lookup("CMAKE_C_COMPILER").ends_with("aarch64-conda-linux-gnu-gcc"));
        assert!(lookup("CMAKE_CXX_COMPILER").ends_with("aarch64-conda-linux-gnu-g++"));
        assert!(lookup("CMAKE_AR").ends_with("aarch64-conda-linux-gnu-ar"));
        assert!(lookup("CMAKE_RANLIB").ends_with("aarch64-conda-linux-gnu-ranlib"));
        assert!(lookup("CMAKE_LINKER").ends_with("aarch64-conda-linux-gnu-ld"));
        let keys = env_keys_for_target("aarch64-unknown-linux-gnu");
        for (key, _) in &env {
            assert!(
                keys.contains(key),
                "environment key {key} missing from plan helper"
            );
        }
        assert!(supports_glibc_floor(
            "aarch64-unknown-linux-gnu",
            GNU_LINUX_GLIBC_BASELINE
        ));
        assert!(!supports_glibc_floor("aarch64-unknown-linux-gnu", "2.28"));
        assert!(!supports_glibc_floor(
            "x86_64-unknown-linux-musl",
            GNU_LINUX_GLIBC_BASELINE
        ));
    });
}
