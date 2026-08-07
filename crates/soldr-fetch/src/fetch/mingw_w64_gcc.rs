//! Managed MinGW-w64 GCC bundle for Windows GNU Rust targets.
//!
//! The first supported target is `x86_64-pc-windows-gnu` on Windows
//! x64 hosts. The bundle is produced by `zackees/soldr-toolchain`
//! from the WinLibs standalone MinGW-w64 GCC zip and exposes a
//! relocatable package root with `bin/gcc.exe`, `bin/g++.exe`,
//! binutils, headers, import libraries, and runtime DLLs.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

pub const MANAGED_MINGW_W64_GCC_VERSION: &str = "15.3.0posix-14.0.0-msvcrt-r1";
pub const MINGW_W64_GCC_TOOL: &str = "mingw-w64-gcc";
pub const MINGW_W64_GCC_TARGET: &str = "x86_64-pc-windows-gnu";
pub const MINGW_W64_GCC_SLUG: &str = "windows-x64-gnu";

/// Host-neutral MinGW-w64 sysroot (soldr-toolchain#114 / soldr#2336):
/// headers, import libraries, CRT startup objects, and the gcc runtime,
/// with **no** host executables. Unlike [`MINGW_W64_GCC_TOOL`] (Windows
/// `.exe` toolchain), this bundle is consumable from any host, so a
/// non-Windows host can materialize the win-gnu link inputs. Pinned to
/// the same WinLibs release + slug as the compiler bundle so CRT/gcc/
/// mingw versions agree.
pub const MINGW_W64_SYSROOT_TOOL: &str = "mingw-w64-sysroot";

/// Linux-hosted MinGW-w64 **cross** gcc toolchain (soldr-toolchain#114
/// Phase 2 / soldr#2336). A relocatable `x86_64-w64-mingw32-{gcc,g++,ar,
/// ranlib,dlltool,windres,ld}` toolchain built from conda-forge, so a
/// Linux host can compile + link win-gnu with a real gcc driver (the
/// maintainer-chosen path — gcc kept, not zig). Published under the HOST
/// platform slug (where the toolchain runs), like `gnu-linux-toolchain`.
pub const MINGW_W64_CROSS_TOOL: &str = "mingw-w64-cross";
pub const MINGW_W64_CROSS_VERSION: &str = "mingw-w64-gcc-15.3.0";
pub const MINGW_W64_CROSS_HOST_SLUG: &str = "linux-x64-gnu";
/// Cross-tool basename prefix inside the bundle (`bin/<prefix>-gcc`).
pub const MINGW_W64_CROSS_PREFIX: &str = "x86_64-w64-mingw32";

pub fn current_host_supports_mingw_w64_gcc() -> bool {
    cfg!(all(target_os = "windows", target_arch = "x86_64"))
}

/// The Linux-hosted mingw cross toolchain is published for the
/// `linux-x64-gnu` host only (Phase 2). Other non-Windows hosts (macOS,
/// linux-arm) have no cross bundle yet.
pub fn current_host_supports_mingw_w64_cross() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

/// Catalogue URL for the managed MinGW bundle.
pub fn mingw_w64_gcc_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_GCC_TOOL, version, slug)
}

pub fn slug_for_target(target_triple: &str) -> Option<&'static str> {
    match target_triple {
        MINGW_W64_GCC_TARGET => Some(MINGW_W64_GCC_SLUG),
        _ => None,
    }
}

pub async fn ensure_mingw_w64_gcc(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = slug_for_target(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "managed MinGW-w64 GCC is only available for {MINGW_W64_GCC_TARGET}; got {target_triple}"
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_GCC_TOOL,
        MANAGED_MINGW_W64_GCC_VERSION,
        slug,
    )
    .await
}

/// Fetch the host-neutral MinGW-w64 sysroot bundle
/// (soldr-toolchain#114). Returns the bundle root; the target sysroot
/// lives under `package/x86_64-w64-mingw32/` with the gcc runtime under
/// `package/lib/gcc/x86_64-w64-mingw32/`. Usable from any host — this is
/// the fetch a non-Windows-host win-gnu branch consumes instead of the
/// Windows-only compiler bundle.
pub async fn ensure_mingw_w64_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let slug = slug_for_target(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "managed MinGW-w64 sysroot is only available for {MINGW_W64_GCC_TARGET}; got {target_triple}"
        ))
    })?;
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_SYSROOT_TOOL,
        MANAGED_MINGW_W64_GCC_VERSION,
        slug,
    )
    .await
}

/// Catalogue URL for the host-neutral sysroot bundle.
pub fn mingw_w64_sysroot_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_SYSROOT_TOOL, version, slug)
}

/// Fetch the Linux-hosted mingw cross gcc toolchain
/// (soldr-toolchain#114 Phase 2). Returns the bundle root; the cross
/// tools live under `bin/x86_64-w64-mingw32-*` and the toolchain carries
/// its own mingw sysroot, so the gcc driver resolves headers/CRT itself.
pub async fn ensure_mingw_w64_cross(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    super::syslib_common::ensure_syslib_bundle(
        paths,
        MINGW_W64_CROSS_TOOL,
        MINGW_W64_CROSS_VERSION,
        MINGW_W64_CROSS_HOST_SLUG,
    )
    .await
}

/// Catalogue URL for the Linux-hosted mingw cross toolchain bundle.
pub fn mingw_w64_cross_asset_url_for(version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for(MINGW_W64_CROSS_TOOL, version, slug)
}

/// Path to a cross tool inside a materialized cross-toolchain bundle:
/// `bin/x86_64-w64-mingw32-<tool>`. The host is Linux x64 (gated by
/// [`current_host_supports_mingw_w64_cross`]), so no `.exe` suffix.
pub fn cross_tool_path(bundle_root: &Path, tool: &str) -> PathBuf {
    bin_dir(bundle_root).join(format!("{MINGW_W64_CROSS_PREFIX}-{tool}"))
}

/// Target-scoped env for a Linux-hosted win-gnu build driven by the
/// mingw **cross** toolchain. Same cc-rs / cargo keys as
/// [`env_for_target`], but pointing at the `x86_64-w64-mingw32-*`
/// cross drivers rather than a Windows-host `bin/gcc.exe`.
pub fn cross_env_for_target(bundle_root: &Path, target_triple: &str) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_uppercase();

    let tool = |name: &str| {
        cross_tool_path(bundle_root, name)
            .to_string_lossy()
            .into_owned()
    };
    vec![
        (
            "MINGW_W64_CROSS_ROOT".to_string(),
            bundle_root.to_string_lossy().into_owned(),
        ),
        (
            "MINGW_W64_CROSS_BIN".to_string(),
            bin_dir(bundle_root).to_string_lossy().into_owned(),
        ),
        (format!("CC_{target_u}"), tool("gcc")),
        (format!("CXX_{target_u}"), tool("g++")),
        (format!("AR_{target_u}"), tool("ar")),
        (format!("RANLIB_{target_u}"), tool("ranlib")),
        (format!("WINDRES_{target_u}"), tool("windres")),
        (format!("DLLTOOL_{target_u}"), tool("dlltool")),
        (format!("CARGO_TARGET_{target_u_upper}_LINKER"), tool("gcc")),
    ]
}

/// The `x86_64-w64-mingw32/lib` directory inside a materialized sysroot
/// (or compiler) bundle root — where CRT startup objects (`crt2.o`,
/// `dllcrt2.o`) and import libraries live.
pub fn sysroot_lib_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("x86_64-w64-mingw32").join("lib")
}

/// The `x86_64-w64-mingw32/include` directory inside a materialized
/// bundle root.
pub fn sysroot_include_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("x86_64-w64-mingw32").join("include")
}

pub fn bin_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("bin")
}

/// Restore-completeness check for the managed win-gnu compiler bundle
/// (soldr#2336 gap #3). A bundle that restored only `bin/gcc.exe` but is
/// missing dlltool/windres or the sysroot import libs + headers reads as
/// "present" yet fails at link time; assert the tools AND the sysroot
/// inputs a win-gnu link actually consumes. `bundle_dir` holds the
/// `.complete` marker; `package` is its extracted payload root.
pub fn managed_restore_present(bundle_dir: &Path, package: &Path) -> bool {
    let has_tool = |tool: &str| bin_dir(package).join(exe_name(tool)).is_file();
    bundle_dir.join(".complete").is_file()
        && has_tool("gcc")
        && has_tool("dlltool")
        && has_tool("windres")
        && sysroot_lib_dir(package).join("crt2.o").is_file()
        && sysroot_lib_dir(package).join("libmsvcrt.a").is_file()
        && sysroot_include_dir(package).join("stdio.h").is_file()
}

pub fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

pub fn tool_path(bundle_root: &Path, tool: &str) -> PathBuf {
    bin_dir(bundle_root).join(exe_name(tool))
}

/// Target-scoped env consumed by Cargo, rustc, cc-rs, and resource
/// build scripts for `x86_64-pc-windows-gnu`.
pub fn env_for_target(bundle_root: &Path, target_triple: &str) -> Vec<(String, String)> {
    let target_u = target_triple.replace('-', "_");
    let target_u_upper = target_u.to_uppercase();

    let tool = |name: &str| tool_path(bundle_root, name).to_string_lossy().into_owned();
    vec![
        (
            "MINGW_W64_GCC_ROOT".to_string(),
            bundle_root.to_string_lossy().into_owned(),
        ),
        (
            "MINGW_W64_GCC_BIN".to_string(),
            bin_dir(bundle_root).to_string_lossy().into_owned(),
        ),
        (format!("CC_{target_u}"), tool("gcc")),
        (format!("CXX_{target_u}"), tool("g++")),
        (format!("AR_{target_u}"), tool("ar")),
        (format!("RANLIB_{target_u}"), tool("ranlib")),
        (format!("WINDRES_{target_u}"), tool("windres")),
        // soldr#2336 gap #4: the bundle ships `dlltool`, but it was
        // never surfaced. Rust's windows-gnu link + cc-rs import-lib
        // generation (`.def` -> `.a`) both look for `DLLTOOL_<triple>`.
        (format!("DLLTOOL_{target_u}"), tool("dlltool")),
        (format!("CARGO_TARGET_{target_u_upper}_LINKER"), tool("gcc")),
    ]
}

pub fn path_with_mingw_bin(
    bundle_root: &Path,
    current_path: Option<std::ffi::OsString>,
) -> Result<String, SoldrError> {
    let mut entries = vec![bin_dir(bundle_root)];
    if let Some(path) = current_path {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| SoldrError::Other(format!("failed to build PATH with MinGW bin: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(asset_url_layout_matches_catalogue, {
        let url = mingw_w64_gcc_asset_url_for(MANAGED_MINGW_W64_GCC_VERSION, MINGW_W64_GCC_SLUG);
        assert!(url.contains("/mingw-w64-gcc/"));
        assert!(url.contains("/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
    });

    crate::timed_test!(target_slug_scope_is_explicit, {
        assert_eq!(
            slug_for_target("x86_64-pc-windows-gnu"),
            Some("windows-x64-gnu")
        );
        assert_eq!(slug_for_target("aarch64-pc-windows-gnullvm"), None);
        assert_eq!(slug_for_target("x86_64-pc-windows-msvc"), None);
    });

    crate::timed_test!(host_scope_is_windows_x64_only, {
        assert_eq!(
            current_host_supports_mingw_w64_gcc(),
            cfg!(all(target_os = "windows", target_arch = "x86_64"))
        );
    });

    crate::timed_test!(env_for_target_sets_cargo_and_cc_rs_vars, {
        let root = Path::new("C:/soldr/mingw");
        let env = env_for_target(root, "x86_64-pc-windows-gnu");
        let lookup = |name: &str| {
            env.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_x86_64_pc_windows_gnu").contains("gcc"));
        assert!(lookup("CXX_x86_64_pc_windows_gnu").contains("g++"));
        assert!(lookup("AR_x86_64_pc_windows_gnu").contains("ar"));
        assert!(lookup("WINDRES_x86_64_pc_windows_gnu").contains("windres"));
        // soldr#2336 gap #4: dlltool must be surfaced.
        assert!(lookup("DLLTOOL_x86_64_pc_windows_gnu").contains("dlltool"));
        assert!(lookup("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER").contains("gcc"));
    });

    crate::timed_test!(sysroot_asset_url_layout_matches_catalogue, {
        // soldr-toolchain#114 host-neutral sysroot lives beside the
        // compiler bundle under its own tool subtree, same version/slug.
        let url =
            mingw_w64_sysroot_asset_url_for(MANAGED_MINGW_W64_GCC_VERSION, MINGW_W64_GCC_SLUG);
        assert!(url.contains("/mingw-w64-sysroot/"));
        assert!(url.contains("/15.3.0posix-14.0.0-msvcrt-r1/windows-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
        // The sysroot pins the SAME upstream release as the compiler so
        // CRT/gcc/mingw versions agree.
        assert_eq!(MINGW_W64_SYSROOT_TOOL, "mingw-w64-sysroot");
    });

    crate::timed_test!(sysroot_dirs_resolve_under_target_triple, {
        let root = Path::new("/soldr/mingw-sysroot/package");
        let lib = sysroot_lib_dir(root);
        let include = sysroot_include_dir(root);
        assert!(lib.ends_with("x86_64-w64-mingw32/lib"));
        assert!(include.ends_with("x86_64-w64-mingw32/include"));
    });

    crate::timed_test!(cross_env_points_at_mingw32_prefixed_drivers, {
        // soldr-toolchain#114 Phase 2: Linux-host cross toolchain uses
        // `bin/x86_64-w64-mingw32-*`, not a Windows `bin/gcc.exe`.
        let root = Path::new("/soldr/mingw-cross");
        let env = cross_env_for_target(root, "x86_64-pc-windows-gnu");
        let lookup = |name: &str| {
            env.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert!(lookup("CC_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-gcc"));
        assert!(lookup("CXX_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-g++"));
        assert!(lookup("AR_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-ar"));
        assert!(lookup("DLLTOOL_x86_64_pc_windows_gnu").contains("x86_64-w64-mingw32-dlltool"));
        assert!(
            lookup("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER").contains("x86_64-w64-mingw32-gcc")
        );
    });

    crate::timed_test!(cross_asset_url_uses_host_slug, {
        let url = mingw_w64_cross_asset_url_for(MINGW_W64_CROSS_VERSION, MINGW_W64_CROSS_HOST_SLUG);
        assert!(url.contains("/mingw-w64-cross/"));
        assert!(url.contains("/mingw-w64-gcc-15.3.0/linux-x64-gnu/"));
        assert!(url.ends_with("/bundle.tar.zst"));
    });
}
