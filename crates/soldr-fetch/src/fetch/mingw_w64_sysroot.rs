//! Host-neutral MinGW-w64 sysroot for `x86_64-pc-windows-gnu`.
//!
//! soldr#2336 / soldr-toolchain#114. The `mingw-w64-gcc` bundle
//! ([`super::mingw_w64_gcc`]) ships Windows `.exe` host binaries, so it
//! is only usable **on** a Windows x64 host. A Linux- or macOS-hosted
//! consumer that brings its own linker — e.g. [`zackees/reld`]'s
//! `cross-release` lane, which bridges to `lld` for PE-COFF — cannot run
//! those executables. What it needs is the *sysroot*: CRT startup
//! objects, import libraries, headers, and the gcc runtime, with **no
//! host executables**.
//!
//! The `mingw-w64-sysroot` catalogue tool publishes exactly that,
//! host-independent, under the same slug (`windows-x64-gnu`) and version
//! as the gcc bundle. Its layout (verified against the published asset)
//! is:
//!
//! ```text
//! package/
//!   x86_64-w64-mingw32/
//!     include/            <- headers (windows.h, ...)
//!     lib/                <- import libs + CRT (crt2.o, libkernel32.a, ...)
//!   lib/gcc/x86_64-w64-mingw32/<gcc-ver>/   <- libgcc.a, crtbegin.o, crtend.o
//! ```
//!
//! [`zackees/reld`]: https://github.com/zackees/reld

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

/// Catalogue version — kept in lockstep with the gcc bundle so both
/// halves of the win-gnu toolchain describe the same mingw-w64 release.
pub const MANAGED_MINGW_W64_SYSROOT_VERSION: &str = "15.3.0posix-14.0.0-msvcrt-r1";
pub const MINGW_W64_SYSROOT_TOOL: &str = "mingw-w64-sysroot";
pub const MINGW_W64_SYSROOT_TARGET: &str = "x86_64-pc-windows-gnu";
pub const MINGW_W64_SYSROOT_SLUG: &str = "windows-x64-gnu";

/// The GNU target prefix under which the sysroot's `include/` and `lib/`
/// live. Stable across mingw-w64 releases for this target.
pub const MINGW_TARGET_PREFIX: &str = "x86_64-w64-mingw32";

pub fn slug_for_target(target_triple: &str) -> Option<&'static str> {
    match target_triple {
        MINGW_W64_SYSROOT_TARGET => Some(MINGW_W64_SYSROOT_SLUG),
        _ => None,
    }
}

/// Catalogue URL for the host-neutral sysroot bundle.
pub fn mingw_w64_sysroot_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_SYSROOT_TOOL, version, slug)
}

/// Download + verify + extract the host-neutral sysroot, returning the
/// `package/` root (the directory that directly contains
/// `x86_64-w64-mingw32/` and `lib/`).
pub async fn ensure_mingw_w64_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = slug_for_target(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "host-neutral MinGW-w64 sysroot is only available for \
             {MINGW_W64_SYSROOT_TARGET}; got {target_triple}"
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_SYSROOT_TOOL,
        MANAGED_MINGW_W64_SYSROOT_VERSION,
        slug,
    )
    .await
}

/// The `include/` directory a cross compile searches for win-gnu headers.
pub fn sysroot_include_dir(sysroot_root: &Path) -> PathBuf {
    sysroot_root.join(MINGW_TARGET_PREFIX).join("include")
}

/// The import-library + CRT-object directory (`crt2.o`, `libkernel32.a`,
/// `libmingw32.a`, ...).
pub fn sysroot_lib_dir(sysroot_root: &Path) -> PathBuf {
    sysroot_root.join(MINGW_TARGET_PREFIX).join("lib")
}

/// The gcc runtime directory (`libgcc.a`, `crtbegin.o`, `crtend.o`),
/// which nests a per-gcc-version subdir. Discovered at runtime by
/// scanning `lib/gcc/x86_64-w64-mingw32/*`; falls back to the parent
/// (version-less) directory when nothing has been materialized yet so
/// the value is still meaningful in a pure/offline context.
pub fn gcc_lib_dir(sysroot_root: &Path) -> PathBuf {
    let parent = sysroot_root
        .join("lib")
        .join("gcc")
        .join(MINGW_TARGET_PREFIX);
    if let Ok(read) = std::fs::read_dir(&parent) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("libgcc.a").is_file() {
                return path;
            }
        }
    }
    parent
}

/// Files whose presence proves a *complete* host-neutral sysroot: a
/// header, an import library, a CRT startup object, and the gcc runtime.
/// soldr#2336 item 3 — a restore that dropped any of these must report
/// "missing" rather than passing on a bare directory that links nothing.
/// Verified against the published bundle layout.
pub fn verification_paths(sysroot_root: &Path) -> Vec<PathBuf> {
    vec![
        sysroot_include_dir(sysroot_root).join("windows.h"),
        sysroot_lib_dir(sysroot_root).join("libkernel32.a"),
        sysroot_lib_dir(sysroot_root).join("crt2.o"),
        gcc_lib_dir(sysroot_root).join("libgcc.a"),
    ]
}

/// Env consumed by an external linker (reld) or a hand-rolled cross
/// link when soldr provides only the sysroot (no gcc driver on the
/// host). These are discovery variables — deliberately non-invasive:
/// they never clobber the caller's `RUSTFLAGS`/`CC`, they just publish
/// where the materialized sysroot lives so a linker that knows to look
/// can find CRT objects, import libs, and headers.
pub fn sysroot_env(sysroot_root: &Path) -> Vec<(String, String)> {
    let s = |p: PathBuf| p.to_string_lossy().into_owned();
    vec![
        (
            "MINGW_W64_SYSROOT_ROOT".to_string(),
            s(sysroot_root.to_path_buf()),
        ),
        (
            "MINGW_W64_SYSROOT_INCLUDE".to_string(),
            s(sysroot_include_dir(sysroot_root)),
        ),
        (
            "MINGW_W64_SYSROOT_LIBDIR".to_string(),
            s(sysroot_lib_dir(sysroot_root)),
        ),
        (
            "MINGW_W64_SYSROOT_GCCLIBDIR".to_string(),
            s(gcc_lib_dir(sysroot_root)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let url = mingw_w64_sysroot_asset_url_for(
            MANAGED_MINGW_W64_SYSROOT_VERSION,
            MINGW_W64_SYSROOT_SLUG,
        );
        assert!(url.contains("/mingw-w64-sysroot/"), "{url}");
        assert!(
            url.contains("/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/"),
            "{url}"
        );
        assert!(url.ends_with("/bundle.tar.zst"), "{url}");
    });

    crate::timed_test!(target_slug_scope_is_explicit, {
        assert_eq!(
            slug_for_target("x86_64-pc-windows-gnu"),
            Some("windows-x64-gnu")
        );
        assert_eq!(slug_for_target("aarch64-pc-windows-gnullvm"), None);
        assert_eq!(slug_for_target("x86_64-pc-windows-msvc"), None);
    });

    crate::timed_test!(sysroot_paths_match_published_layout, {
        let root = Path::new("/soldr/mingw-sysroot/package");
        assert!(sysroot_include_dir(root)
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("x86_64-w64-mingw32/include"));
        assert!(sysroot_lib_dir(root)
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("x86_64-w64-mingw32/lib"));
        // No materialized tree here, so gcc_lib_dir falls back to the
        // version-less parent — still a meaningful, well-formed path.
        assert!(gcc_lib_dir(root)
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("lib/gcc/x86_64-w64-mingw32"));
    });

    crate::timed_test!(sysroot_env_publishes_discovery_vars, {
        let root = Path::new("/soldr/mingw-sysroot/package");
        let env = sysroot_env(root);
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert!(get("MINGW_W64_SYSROOT_ROOT").contains("package"));
        assert!(get("MINGW_W64_SYSROOT_INCLUDE")
            .replace('\\', "/")
            .ends_with("include"));
        assert!(get("MINGW_W64_SYSROOT_LIBDIR")
            .replace('\\', "/")
            .ends_with("lib"));
        assert!(get("MINGW_W64_SYSROOT_GCCLIBDIR").contains("gcc"));
    });

    crate::timed_test!(verification_paths_cover_header_importlib_crt_runtime, {
        let root = Path::new("/soldr/mingw-sysroot/package");
        let all = verification_paths(root)
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("x86_64-w64-mingw32/include/windows.h"),
            "{all}"
        );
        assert!(
            all.contains("x86_64-w64-mingw32/lib/libkernel32.a"),
            "{all}"
        );
        assert!(all.contains("x86_64-w64-mingw32/lib/crt2.o"), "{all}");
        assert!(all.contains("libgcc.a"), "{all}");
    });

    crate::timed_test!(gcc_lib_dir_discovers_versioned_subdir, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path();
        let versioned = root
            .join("lib")
            .join("gcc")
            .join(MINGW_TARGET_PREFIX)
            .join("15.3.0");
        std::fs::create_dir_all(&versioned).unwrap();
        std::fs::write(versioned.join("libgcc.a"), b"stub").unwrap();
        assert_eq!(gcc_lib_dir(root), versioned);
    });
}
