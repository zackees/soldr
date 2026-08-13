//! Managed musl Linux cross compiler/linker preparation.
//!
//! GNU Linux uses the catalogue-backed compiler/sysroot lifecycle in
//! `target_lifecycle`. This legacy module is intentionally limited to musl
//! until #2244 supplies an equivalent catalogue-backed musl toolchain.

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
    let zig_target = rust_target_to_zig_target(triple)?;
    let zig_target = zig_target.as_str();
    let zig_dir = crate::fetch::ensure_zig(paths).await?;
    let zig = zig_dir.join(crate::platform::executable::name::native("zig"));
    // The musl triple is part of the directory so the two architectures never
    // share wrapper scripts.
    let wrapper_dir = paths.bin.join("linux-cross").join(triple);
    std::fs::create_dir_all(&wrapper_dir)?;

    let ext = crate::platform::executable::name::script_suffix();
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

/// Map the two supported Rust musl triples to Zig's spelling.
fn rust_target_to_zig_target(triple: &str) -> Result<String, SoldrError> {
    let zig_target = match triple {
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "legacy managed musl preparation does not support target `{triple}`"
            )))
        }
    };
    Ok(zig_target.to_string())
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

    crate::timed_test!(maps_only_legacy_managed_musl_targets, {
        assert_eq!(
            rust_target_to_zig_target("x86_64-unknown-linux-musl").unwrap(),
            "x86_64-linux-musl"
        );
        assert_eq!(
            rust_target_to_zig_target("aarch64-unknown-linux-musl").unwrap(),
            "aarch64-linux-musl"
        );
        assert!(rust_target_to_zig_target("x86_64-unknown-linux-gnu").is_err());
    });

    crate::timed_test!(a_gnu_target_is_rejected_by_the_legacy_musl_wrapper, {
        let err = rust_target_to_zig_target("aarch64-unknown-linux-gnu").unwrap_err();
        assert!(
            err.to_string().contains("aarch64-unknown-linux-gnu"),
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
