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

pub fn current_host_supports_mingw_w64_gcc() -> bool {
    cfg!(all(target_os = "windows", target_arch = "x86_64"))
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

pub fn bin_dir(bundle_root: &Path) -> PathBuf {
    bundle_root.join("bin")
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

/// Files whose presence proves a *complete* MinGW-w64 GCC bundle — not
/// just the compiler driver but the binutils a win-gnu link needs
/// (`dlltool` for import libs, `windres` for resources) and the sysroot
/// itself (a representative header + import library).
///
/// soldr#2336 item 3: `expected_state_paths` used to check only
/// `bin/gcc.exe`, so a truncated restore that dropped the sysroot or the
/// resource/import-lib tools was reported "present" and then failed far
/// later at the actual link. Verified against the published bundle
/// layout: `bin/{gcc,dlltool,windres}.exe`,
/// `x86_64-w64-mingw32/include/windows.h`,
/// `x86_64-w64-mingw32/lib/libkernel32.a`.
pub fn verification_paths(bundle_root: &Path) -> Vec<PathBuf> {
    let sysroot = bundle_root.join(super::mingw_w64_sysroot::MINGW_TARGET_PREFIX);
    vec![
        tool_path(bundle_root, "gcc"),
        tool_path(bundle_root, "dlltool"),
        tool_path(bundle_root, "windres"),
        sysroot.join("include").join("windows.h"),
        sysroot.join("lib").join("libkernel32.a"),
    ]
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
        // soldr#2336 item 4: the bundle ships `dlltool`, but it was never
        // surfaced. cc-rs / import-lib generators and external linkers
        // (reld's win-gnu path) look for `DLLTOOL_<triple>` to build
        // import libraries from a `.def`; without it they fall back to a
        // bare `dlltool` PATH lookup that misses the managed toolchain.
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

    crate::timed_test!(verification_paths_cover_binutils_and_sysroot, {
        let root = Path::new("C:/soldr/mingw/package");
        let paths = verification_paths(root);
        let joined: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        let all = joined.join("\n");
        // Not just the compiler driver, but the import-lib + resource
        // tools and a real header + import lib from the sysroot.
        assert!(all.contains("bin/gcc"), "{all}");
        assert!(all.contains("bin/dlltool"), "{all}");
        assert!(all.contains("bin/windres"), "{all}");
        assert!(
            all.contains("x86_64-w64-mingw32/include/windows.h"),
            "{all}"
        );
        assert!(
            all.contains("x86_64-w64-mingw32/lib/libkernel32.a"),
            "{all}"
        );
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
        assert!(lookup("DLLTOOL_x86_64_pc_windows_gnu").contains("dlltool"));
        assert!(lookup("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER").contains("gcc"));
    });
}
