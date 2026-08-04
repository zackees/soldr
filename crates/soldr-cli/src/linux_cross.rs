//! Managed Linux cross compiler/linker preparation.
//!
//! The public surface is ordinary Cargo through soldr.  Zig is an internal,
//! pinned implementation detail: callers select a Rust target triple and do
//! not invoke cargo-zigbuild or manufacture compiler wrappers themselves.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

#[derive(Debug, Clone)]
pub(crate) struct LinuxCrossTools {
    pub(crate) bin_dir: PathBuf,
    pub(crate) cc: PathBuf,
    pub(crate) cxx: PathBuf,
    pub(crate) ar: PathBuf,
    pub(crate) ranlib: PathBuf,
    pub(crate) linker: PathBuf,
}

pub(crate) async fn prepare(
    paths: &SoldrPaths,
    triple: &str,
) -> Result<LinuxCrossTools, SoldrError> {
    let base = crate::target_alias::split_glibc_floor(triple)
        .map(|(base, _)| base)
        .unwrap_or(triple);
    if let Some(target) =
        crate::fetch::gnu_linux_toolchain::GnuLinuxToolchainTarget::for_triple(base)
    {
        // GNU Linux's blessed route is a catalogue-backed GCC/binutils/sysroot
        // bundle. Do not fall through to managed Zig when an asset is missing:
        // the error must name the unavailable catalogue toolchain.
        let toolchain = crate::fetch::gnu_linux_toolchain::ensure(paths, base).await?;
        debug_assert_eq!(toolchain.target, target);
        return Ok(LinuxCrossTools {
            bin_dir: toolchain.bin_dir.clone(),
            cc: toolchain.tool_path("gcc"),
            cxx: toolchain.tool_path("g++"),
            ar: toolchain.tool_path("ar"),
            ranlib: toolchain.tool_path("ranlib"),
            linker: toolchain.tool_path("gcc"),
        });
    }

    let zig_target = rust_target_to_zig_target(triple)?;
    let zig_target = zig_target.as_str();
    let zig_dir = crate::fetch::ensure_zig(paths).await?;
    let zig = zig_dir.join(if cfg!(windows) { "zig.exe" } else { "zig" });
    // Keyed on the full triple including any `.<glibc>` suffix, so a 2.17
    // request and a default-floor request do not share wrapper scripts.
    let wrapper_dir = paths.bin.join("linux-cross").join(triple);
    std::fs::create_dir_all(&wrapper_dir)?;

    let ext = if cfg!(windows) { ".cmd" } else { "" };
    let cc = wrapper_dir.join(format!("cc{ext}"));
    let cxx = wrapper_dir.join(format!("cxx{ext}"));
    let ar = wrapper_dir.join(format!("ar{ext}"));
    let ranlib = wrapper_dir.join(format!("ranlib{ext}"));
    let linker = wrapper_dir.join(format!("linker{ext}"));

    write_compiler_wrapper(&cc, &zig, "cc", zig_target)?;
    write_compiler_wrapper(&cxx, &zig, "c++", zig_target)?;
    write_tool_wrapper(&ar, &zig, "ar")?;
    write_tool_wrapper(&ranlib, &zig, "ranlib")?;
    write_compiler_wrapper(&linker, &zig, "cc", zig_target)?;

    Ok(LinuxCrossTools {
        bin_dir: zig_dir,
        cc,
        cxx,
        ar,
        ranlib,
        linker,
    })
}

/// Map a Rust triple to zig's spelling, carrying an optional glibc floor.
///
/// soldr#2139: `x86_64-unknown-linux-gnu.2.17` becomes `x86_64-linux-gnu.2.17`.
/// Zig takes the floor natively and *enforces* it -- a call into a newer symbol
/// fails at link naming that symbol, rather than producing a binary that only
/// fails on the old system it was built for. That is the whole value of asking
/// for a floor, and it is why the suffix is passed through here rather than
/// dropped somewhere upstream.
///
/// The floor is a request, not a guarantee soldr can make: the achievable floor
/// is `max(this, every symbol the consumer's vendored C dependencies reference)`.
/// A graph pulling in libsqlite3-sys will still fail on `fcntl64` (glibc 2.28)
/// no matter what is asked for here -- loudly, at link, naming the cause.
fn rust_target_to_zig_target(triple: &str) -> Result<String, SoldrError> {
    let (base, floor) = match crate::target_alias::split_glibc_floor(triple) {
        Some((base, floor)) => (base, Some(floor)),
        None => (triple, None),
    };
    let zig_base = match base {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "managed Linux cross preparation does not support target `{triple}`"
            )))
        }
    };
    match floor {
        // Only the -gnu targets have a glibc to floor. A suffix cannot reach a
        // musl base -- `split_glibc_floor` anchors on `-linux-gnu` -- so this
        // is unreachable rather than silently ignored.
        Some(floor) => Ok(format!("{zig_base}.{floor}")),
        None => Ok(zig_base.to_string()),
    }
}

fn write_compiler_wrapper(
    path: &Path,
    zig: &Path,
    subcommand: &str,
    zig_target: &str,
) -> Result<(), SoldrError> {
    let body = if cfg!(windows) {
        format!(
            "@echo off\r\n\
             setlocal EnableDelayedExpansion\r\n\
             set \"filtered=\"\r\n\
             :next_arg\r\n\
             if \"%~1\"==\"\" goto run\r\n\
             set \"arg=%~1\"\r\n\
             if /I \"!arg!\"==\"--target\" (shift & shift & goto next_arg)\r\n\
             if /I \"!arg:~0,9!\"==\"--target=\" (shift & goto next_arg)\r\n\
             set filtered=!filtered! \"%~1\"\r\n\
             shift\r\n\
             goto next_arg\r\n\
             :run\r\n\
             \"{}\" {subcommand} -target {zig_target} !filtered!\r\n",
            zig.display()
        )
    } else {
        format!(
            "#!/usr/bin/env bash\n\
             # cc-rs may pass the Rust triple; Zig needs its own target spelling.\n\
             filtered=()\n\
             for arg in \"$@\"; do\n\
             \tcase \"$arg\" in --target=*) ;; *) filtered+=(\"$arg\") ;; esac\n\
             done\n\
             exec '{}' {subcommand} -target {zig_target} \"${{filtered[@]}}\"\n",
            shell_single_quote(zig)
        )
    };
    write_executable(path, &body)
}

fn write_tool_wrapper(path: &Path, zig: &Path, subcommand: &str) -> Result<(), SoldrError> {
    let body = if cfg!(windows) {
        format!("@echo off\r\n\"{}\" {subcommand} %*\r\n", zig.display())
    } else {
        format!(
            "#!/bin/sh\nexec '{}' {subcommand} \"$@\"\n",
            shell_single_quote(zig)
        )
    };
    write_executable(path, &body)
}

fn shell_single_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

fn write_executable(path: &Path, body: &str) -> Result<(), SoldrError> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(maps_every_managed_linux_cross_target, {
        assert_eq!(
            rust_target_to_zig_target("x86_64-unknown-linux-gnu").unwrap(),
            "x86_64-linux-gnu"
        );
        assert_eq!(
            rust_target_to_zig_target("aarch64-unknown-linux-gnu").unwrap(),
            "aarch64-linux-gnu"
        );
        assert_eq!(
            rust_target_to_zig_target("x86_64-unknown-linux-musl").unwrap(),
            "x86_64-linux-musl"
        );
        assert_eq!(
            rust_target_to_zig_target("aarch64-unknown-linux-musl").unwrap(),
            "aarch64-linux-musl"
        );
        assert!(rust_target_to_zig_target("i686-unknown-linux-gnu").is_err());
    });

    // soldr#2139. The floor has to survive the mapping, because this is the
    // only place that can enforce it -- if it is dropped here the build still
    // succeeds and silently ships the wrong floor.
    crate::timed_test!(a_glibc_floor_is_carried_into_the_zig_target, {
        assert_eq!(
            rust_target_to_zig_target("x86_64-unknown-linux-gnu.2.17").unwrap(),
            "x86_64-linux-gnu.2.17"
        );
        assert_eq!(
            rust_target_to_zig_target("aarch64-unknown-linux-gnu.2.28").unwrap(),
            "aarch64-linux-gnu.2.28"
        );
    });

    crate::timed_test!(an_unsupported_base_is_still_rejected_with_its_suffix, {
        // The error must name what the user typed, suffix included, rather
        // than a base triple they never asked for.
        let err = rust_target_to_zig_target("i686-unknown-linux-gnu.2.17").unwrap_err();
        assert!(
            err.to_string().contains("i686-unknown-linux-gnu.2.17"),
            "{err}"
        );
    });

    crate::timed_test!(unix_wrapper_uses_managed_zig_directly, {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("cc");
        write_compiler_wrapper(
            &wrapper,
            Path::new("/managed tools/zig"),
            "cc",
            "aarch64-linux-gnu",
        )
        .unwrap();
        let body = std::fs::read_to_string(wrapper).unwrap();
        assert!(body.contains("/managed tools/zig"));
        assert!(body.contains("-target aarch64-linux-gnu"));
        assert!(body.contains("--target=*"));
        assert!(
            !body.contains("cargo-zigbuild"),
            "the blessed wrapper must invoke managed Zig directly"
        );
    });

    crate::timed_test!(windows_wrapper_filters_rust_target_spelling, {
        if !cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("cc.cmd");
        write_compiler_wrapper(
            &wrapper,
            Path::new(r"C:\managed tools\zig.exe"),
            "cc",
            "aarch64-linux-gnu",
        )
        .unwrap();
        let body = std::fs::read_to_string(wrapper).unwrap();
        assert!(body.contains("\"--target\""));
        assert!(body.contains("\"--target=\""));
        assert!(body.contains("-target aarch64-linux-gnu"));
    });
}
