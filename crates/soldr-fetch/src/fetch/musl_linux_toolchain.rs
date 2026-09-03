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
        format!("PKG_CONFIG_ALLOW_CROSS_{target_triple}"),
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
        // soldr#3081: deliberately NOT `PKG_CONFIG_SYSROOT_DIR`. pkg-config
        // 0.29.2 prepends that value to *every* absolute `-L`/`-I` it emits,
        // including the ones that come from soldr's own managed syslib `.pc`
        // files on `PKG_CONFIG_PATH_<triple>`. Those use a relocatable
        // `prefix=${pcfiledir}/../..`, so their paths are already absolute and
        // already correct; prefixing them produced
        // `<musl sysroot><absolute bzip2 path>`, a directory that does not
        // exist, and the musl link died with `ld: cannot find -lbz2`. The
        // catalogue musl bundle ships no `.pc` files of its own, so the
        // variable had nothing legitimate to rewrite in the first place.
        //
        // `PKG_CONFIG_ALLOW_CROSS` replaces the one *other* job the sysroot
        // variable was doing by accident: pkg-config-rs's `target_supported()`
        // refuses to probe at all when HOST != TARGET unless `PKG_CONFIG`,
        // `PKG_CONFIG_SYSROOT_DIR` or `PKG_CONFIG_ALLOW_CROSS` is set. Without
        // it, dropping the sysroot variable would silently disable the entire
        // syslib substitution path for cross musl builds. Target-scoped, for
        // the same reason `PKG_CONFIG_PATH_<triple>` is: an unscoped value
        // leaks into host build-script compiles.
        (
            format!("PKG_CONFIG_ALLOW_CROSS_{target_triple}"),
            "1".to_string(),
        ),
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

    #[test]
    fn maps_only_supported_musl_targets() {
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
    }

    #[test]
    fn target_env_is_managed_and_sysrooted() {
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
        // soldr#3081: the managed sysroot is a compiler/CMake concern, never a
        // pkg-config path-rewriting prefix.
        assert!(
            !env.iter().any(|(key, _)| key == "PKG_CONFIG_SYSROOT_DIR"),
            "PKG_CONFIG_SYSROOT_DIR corrupts absolute syslib -L paths (soldr#3081)"
        );
        assert_eq!(
            lookup("PKG_CONFIG_ALLOW_CROSS_aarch64-unknown-linux-musl"),
            "1",
            "cross pkg-config probing must stay enabled without the sysroot var"
        );
        assert!(lookup("PKG_CONFIG_LIBDIR").contains("/lib/pkgconfig"));
        for (key, _) in &env {
            assert!(env_keys_for_target("aarch64-unknown-linux-musl").contains(key));
        }
        assert!(
            asset_url_for(MUSL_LINUX_TOOLCHAIN_VERSION, "linux-arm64-musl")
                .contains("/musl-linux-toolchain/")
        );
    }

    #[test]
    fn incomplete_bundle_fails_with_the_missing_managed_path() {
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
    }

    /// soldr#3081, the reported bug end to end: run the **real** `pkg-config`
    /// against a `.pc` shaped exactly like soldr's managed syslib recipes
    /// (`prefix=${pcfiledir}/../..`, so `-L` comes out absolute) with the
    /// environment `env_for_target` produces, and assert the absolute `-L`
    /// survives unchanged.
    ///
    /// The control half of the test sets `PKG_CONFIG_SYSROOT_DIR` by hand and
    /// asserts the prefix *is* glued on -- that is the third-party behaviour
    /// this fix routes around, and the exact shape of the malformed flag in
    /// the issue.
    #[test]
    fn absolute_syslib_link_paths_survive_pkg_config() {
        let Some(pkg_config) = pkg_config_on_path() else {
            eprintln!("skipping: pkg-config is not on PATH");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let package = dir.path().join("package");
        let pkgconfig_dir = package.join("lib").join("pkgconfig");
        std::fs::create_dir_all(&pkgconfig_dir).expect("create fixture pkgconfig dir");
        std::fs::write(
            pkgconfig_dir.join("soldrfixture.pc"),
            "prefix=${pcfiledir}/../..\n\
             exec_prefix=${prefix}\n\
             libdir=${prefix}/lib\n\
             includedir=${prefix}/include\n\
             \n\
             Name: soldrfixture\n\
             Description: relocatable syslib recipe fixture\n\
             Version: 1.0.0\n\
             Libs: -L${libdir} -lsoldrfixture\n\
             Cflags: -I${includedir}\n",
        )
        .expect("write fixture .pc");

        let sysroot = dir.path().join("musl-sysroot");
        let toolchain = MuslLinuxToolchain {
            root: dir.path().to_path_buf(),
            bin_dir: dir.path().join("bin"),
            sysroot: sysroot.clone(),
            target: MuslLinuxToolchainTarget::X86_64,
        };
        let target_triple = "x86_64-unknown-linux-musl";
        let mut command = std::process::Command::new(&pkg_config);
        command.env_clear();
        command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        command.env("PKG_CONFIG_PATH", &pkgconfig_dir);
        for (key, value) in env_for_target(&toolchain, target_triple) {
            if key.starts_with("PKG_CONFIG_") {
                command.env(key, value);
            }
        }
        let output = command
            .args(["--libs", "soldrfixture"])
            .output()
            .expect("run pkg-config");
        assert!(
            output.status.success(),
            "pkg-config failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let libs = String::from_utf8(output.stdout).expect("utf-8 pkg-config output");
        let link_dir = link_search_dir(&libs).expect("pkg-config emitted no -L");
        assert_eq!(
            std::fs::canonicalize(&link_dir).expect("the emitted -L must exist"),
            std::fs::canonicalize(package.join("lib")).expect("canonicalize fixture lib dir"),
            "absolute syslib -L was rewritten: {libs}"
        );
        assert!(
            !libs.contains(&sysroot.display().to_string()),
            "the musl sysroot was glued onto an already-absolute -L: {libs}"
        );

        // Control: with PKG_CONFIG_SYSROOT_DIR set -- what soldr used to
        // export -- pkg-config concatenates the two absolute paths.
        //
        // `PKG_CONFIG_FDO_SYSROOT_RULES` asks pkgconf (1.9+ narrowed the
        // default to `.pc` files that live *inside* the sysroot) for the
        // freedesktop rules this control case is about; freedesktop
        // pkg-config ignores the variable. An implementation that still
        // declines to prefix has nothing to demonstrate, so the control half
        // reports and returns rather than failing a lane over a third-party
        // default -- the assertion that guards the fix is the one above.
        let mut sysrooted = std::process::Command::new(&pkg_config);
        sysrooted.env_clear();
        sysrooted.env("PATH", std::env::var_os("PATH").unwrap_or_default());
        sysrooted.env("PKG_CONFIG_PATH", &pkgconfig_dir);
        sysrooted.env("PKG_CONFIG_SYSROOT_DIR", &sysroot);
        sysrooted.env("PKG_CONFIG_FDO_SYSROOT_RULES", "1");
        let control = sysrooted
            .args(["--libs", "soldrfixture"])
            .output()
            .expect("run pkg-config with a sysroot");
        assert!(control.status.success());
        let control_libs = String::from_utf8(control.stdout).expect("utf-8 pkg-config output");
        let control_dir = link_search_dir(&control_libs).expect("pkg-config emitted no -L");
        if !control_dir.starts_with(&sysroot) {
            eprintln!(
                "note: this pkg-config does not apply freedesktop sysroot rules to a .pc \
                 outside the sysroot; control half skipped"
            );
            return;
        }
        assert!(
            !control_dir.exists(),
            "the sysroot-prefixed -L is the nonexistent directory from soldr#3081"
        );
    }

    /// The directory from the first `-L` flag in a `pkg-config --libs` line.
    fn link_search_dir(libs: &str) -> Option<std::path::PathBuf> {
        libs.split_whitespace()
            .find_map(|word| word.strip_prefix("-L"))
            .map(std::path::PathBuf::from)
    }

    fn pkg_config_on_path() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(format!("pkg-config{}", std::env::consts::EXE_SUFFIX)))
            .find(|candidate| candidate.is_file())
    }
}
