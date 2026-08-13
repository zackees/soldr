//! Managed CMake + Ninja bundle fetchers — soldr's answer to "use
//! whatever cmake/make happens to be on PATH".
//!
//! Motivation (2026-07-01): a `pip install soldr` build on a Windows
//! dev box died inside `libz-ng-sys`'s cmake configure step because
//! the system PATH resolved `cmake` to a pip-installed wheel and
//! `make` to the MSYS-flavored binary bundled by an unrelated pip
//! package (`zcmds_win32`). CMake picked the "MSYS Makefiles"
//! generator, the MSYS make path-mangled MSVC flags (`/nologo` →
//! `C:\Program Files\Git\nologo.obj`), and the try-compile exploded.
//! None of that is soldr's PATH to fix — but soldr builds should
//! never have been at its mercy in the first place.
//!
//! These fetchers pull pinned upstream-official binaries (Kitware
//! CMake, ninja-build ninja) repackaged as soldr-toolchain catalogue
//! bundles, so the blessed `soldr build` path can export a known-good
//! `CMAKE` + `CMAKE_GENERATOR=Ninja` combination to every cmake-based
//! `*-sys` build script (libz-ng-sys, zstd-sys, ...) instead of
//! trusting the machine. See `blessed_build::inject_cmake_tooling`
//! for the env-injection side.
//!
//! Bundle layout (forge recipes `cmake-<shape>` / `ninja-<shape>` on
//! zackees/soldr-toolchain):
//!
//! ```text
//! cmake bundle: bin/cmake(.exe) bin/ctest(.exe) bin/cpack(.exe)
//!               share/cmake-<major.minor>/...   ← module tree
//! ninja bundle: bin/ninja(.exe)
//! ```
//!
//! Stub-until-ingested: like every other catalogue consumer, when the
//! catalogue has no row for (tool, version, slug) the ensure fns error
//! and the caller falls through to system cmake — nothing breaks
//! before the assets land.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

/// CMake version pinned by the soldr-toolchain `cmake-<shape>` recipes.
pub const MANAGED_CMAKE_VERSION: &str = "4.3.4";

/// Ninja version pinned by the soldr-toolchain `ninja-<shape>` recipes.
pub const MANAGED_NINJA_VERSION: &str = "1.13.2";

/// Host triple → catalogue shape slug. cmake + ninja are HOST tools
/// (they run on the build machine regardless of the compile target),
/// so the table keys on the host, not the `--target`.
///
/// musl hosts are deliberately absent: the upstream Kitware / ninja
/// linux binaries are glibc-linked. Alpine-style hosts fall through
/// to system cmake.
pub const CMAKE_TOOL_HOSTS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
];

/// Catalogue slug for a host triple, if the host is supported.
pub fn host_slug_for(host_triple: &str) -> Option<&'static str> {
    CMAKE_TOOL_HOSTS
        .iter()
        .find(|(rust, _)| *rust == host_triple)
        .map(|(_, slug)| *slug)
}

/// The running binary's host triple, resolved at runtime from the
/// host-facts facade. Mirrors the supported-host set of
/// [`CMAKE_TOOL_HOSTS`] plus musl (which then misses the slug lookup
/// and falls through — the miss is the mechanism, not an error path
/// we need to special-case).
pub fn current_host_triple() -> &'static str {
    use crate::platform::host::facts::{arch, libc, os, HostArch, HostLibc, HostOs};

    match (os(), arch(), libc()) {
        (HostOs::Windows, HostArch::X86_64, _) => "x86_64-pc-windows-msvc",
        (HostOs::Windows, HostArch::Aarch64, _) => "aarch64-pc-windows-msvc",
        (HostOs::MacOs, HostArch::X86_64, _) => "x86_64-apple-darwin",
        (HostOs::MacOs, HostArch::Aarch64, _) => "aarch64-apple-darwin",
        (HostOs::Linux, HostArch::X86_64, HostLibc::Musl) => "x86_64-unknown-linux-musl",
        (HostOs::Linux, HostArch::Aarch64, HostLibc::Musl) => "aarch64-unknown-linux-musl",
        (HostOs::Linux, HostArch::X86_64, _) => "x86_64-unknown-linux-gnu",
        (HostOs::Linux, HostArch::Aarch64, _) => "aarch64-unknown-linux-gnu",
        _ => "unsupported-host",
    }
}

/// Assets-branch URL for the cmake bundle. Catalogue layout:
/// `cmake/<version>/<slug>/bundle.tar.zst`.
pub fn cmake_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("cmake", version, slug)
}

/// Assets-branch URL for the ninja bundle. Catalogue layout:
/// `ninja/<version>/<slug>/bundle.tar.zst`.
pub fn ninja_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("ninja", version, slug)
}

/// Materialize the managed CMake bundle for `host_triple`. Returns
/// the bundle root (contains `bin/` + `share/`).
pub async fn ensure_cmake_bundle(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = host_slug_for(host_triple).ok_or_else(|| unsupported_host("cmake", host_triple))?;
    super::syslib_common::ensure_syslib_bundle(paths, "cmake", MANAGED_CMAKE_VERSION, slug).await
}

/// Materialize the managed Ninja bundle for `host_triple`. Returns
/// the bundle root (contains `bin/`).
pub async fn ensure_ninja_bundle(
    paths: &SoldrPaths,
    host_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = host_slug_for(host_triple).ok_or_else(|| unsupported_host("ninja", host_triple))?;
    super::syslib_common::ensure_syslib_bundle(paths, "ninja", MANAGED_NINJA_VERSION, slug).await
}

fn unsupported_host(tool: &str, host_triple: &str) -> SoldrError {
    SoldrError::UnsupportedPlatform(format!(
        "no managed {tool} bundle for host {host_triple}; supported: {:?}",
        CMAKE_TOOL_HOSTS.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    ))
}

/// Path to the cmake executable inside a materialized bundle root.
pub fn cmake_exe(bundle_root: &Path) -> PathBuf {
    bundle_root.join("bin").join(exe_name("cmake"))
}

/// Path to the ninja executable inside a materialized bundle root.
pub fn ninja_exe(bundle_root: &Path) -> PathBuf {
    bundle_root.join("bin").join(exe_name("ninja"))
}

fn exe_name(base: &str) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(host_slug_for_supported_triples, {
        assert_eq!(host_slug_for("x86_64-pc-windows-msvc"), Some("windows-x64"));
        assert_eq!(host_slug_for("aarch64-apple-darwin"), Some("darwin-arm64"));
        assert_eq!(
            host_slug_for("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        // musl hosts intentionally unsupported — glibc-linked upstream
        // binaries. The miss routes callers to system cmake.
        assert_eq!(host_slug_for("x86_64-unknown-linux-musl"), None);
        assert_eq!(host_slug_for("wasm32-unknown-unknown"), None);
    });

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let c = cmake_asset_url_for(MANAGED_CMAKE_VERSION, "windows-x64");
        assert!(c.starts_with("https://media.githubusercontent.com/media/"));
        assert!(c.contains("/cmake/4.3.4/windows-x64/"));
        assert!(c.ends_with("/bundle.tar.zst"));

        let n = ninja_asset_url_for(MANAGED_NINJA_VERSION, "linux-arm64-gnu");
        assert!(n.contains("/ninja/1.13.2/linux-arm64-gnu/"));
        assert!(n.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(version_constants_well_formed, {
        for v in [MANAGED_CMAKE_VERSION, MANAGED_NINJA_VERSION] {
            let parts: Vec<&str> = v.split('.').collect();
            assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH, got {v}");
            for p in parts {
                assert!(p.chars().all(|c| c.is_ascii_digit()), "non-digit in {v}");
            }
        }
    });

    crate::timed_test!(current_host_triple_is_in_known_set_or_musl, {
        let host = current_host_triple();
        let known = CMAKE_TOOL_HOSTS.iter().any(|(t, _)| *t == host);
        let musl = host.ends_with("-unknown-linux-musl");
        assert!(
            known || musl,
            "host {host} neither a supported cmake host nor musl"
        );
    });

    crate::timed_test!(exe_paths_are_platform_correct, {
        let root = Path::new("root");
        let cmake = cmake_exe(root);
        let ninja = ninja_exe(root);
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(cmake.ends_with("bin/cmake.exe") || cmake.ends_with("bin\\cmake.exe"));
            assert!(ninja.ends_with("bin/ninja.exe") || ninja.ends_with("bin\\ninja.exe"));
        } else {
            assert!(cmake.ends_with("bin/cmake"));
            assert!(ninja.ends_with("bin/ninja"));
        }
    });

    crate::timed_test!(ensure_bundles_reject_unsupported_host, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(ensure_cmake_bundle(&paths, "wasm32-unknown-unknown"))
            .expect_err("unsupported host must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
        let err = rt
            .block_on(ensure_ninja_bundle(&paths, "x86_64-unknown-linux-musl"))
            .expect_err("musl host must error");
        assert!(matches!(err, SoldrError::UnsupportedPlatform(_)));
    });
}
