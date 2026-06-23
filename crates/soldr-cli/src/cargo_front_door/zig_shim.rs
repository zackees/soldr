//! Per-target Zig compiler wrappers for `soldr cargo zigbuild`.
//!
//! `cargo-zigbuild` prepares a similar shape for its own inner cargo
//! invocation. soldr also needs process-side env exports so build scripts
//! and nested cargo calls see the same target linker policy.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

const SHIM_DIR_BASENAME: &str = "zigbuild-shims";

pub(super) struct ZigWrappers {
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    pub ranlib: PathBuf,
}

pub(super) fn ensure_zig_wrappers(
    paths: &SoldrPaths,
    triple: &str,
) -> Result<ZigWrappers, SoldrError> {
    let zig_target = rust_target_to_zig_target(triple)?;
    let dir = paths.bin.join(SHIM_DIR_BASENAME).join(triple);
    std::fs::create_dir_all(&dir)?;

    let ext = if cfg!(windows) { ".cmd" } else { "" };
    let cc = dir.join(format!("cc{ext}"));
    let cxx = dir.join(format!("cxx{ext}"));
    let ar = dir.join(format!("ar{ext}"));
    let ranlib = dir.join(format!("ranlib{ext}"));

    write_wrapper(&cc, &render_cc_wrapper("cc", zig_target))?;
    write_wrapper(&cxx, &render_cc_wrapper("c++", zig_target))?;
    write_wrapper(&ar, &render_tool_wrapper("ar"))?;
    write_wrapper(&ranlib, &render_tool_wrapper("ranlib"))?;

    Ok(ZigWrappers {
        cc,
        cxx,
        ar,
        ranlib,
    })
}

fn rust_target_to_zig_target(triple: &str) -> Result<&'static str, SoldrError> {
    match triple {
        "x86_64-unknown-linux-gnu" => Ok("x86_64-linux-gnu"),
        "aarch64-unknown-linux-gnu" => Ok("aarch64-linux-gnu"),
        "x86_64-unknown-linux-musl" => Ok("x86_64-linux-musl"),
        "aarch64-unknown-linux-musl" => Ok("aarch64-linux-musl"),
        "x86_64-apple-darwin" => Ok("x86_64-macos-none"),
        "aarch64-apple-darwin" => Ok("aarch64-macos-none"),
        _ => Err(SoldrError::UnsupportedPlatform(format!(
            "soldr cargo zigbuild env bootstrap does not support target `{triple}`"
        ))),
    }
}

fn render_cc_wrapper(subcommand: &str, zig_target: &str) -> String {
    if cfg!(windows) {
        format!("@echo off\r\ncargo-zigbuild zig {subcommand} -- -target {zig_target} %*\r\n",)
    } else {
        format!("#!/bin/sh\nexec cargo-zigbuild zig {subcommand} -- -target {zig_target} \"$@\"\n",)
    }
}

fn render_tool_wrapper(subcommand: &str) -> String {
    if cfg!(windows) {
        format!("@echo off\r\ncargo-zigbuild zig {subcommand} -- %*\r\n")
    } else {
        format!("#!/bin/sh\nexec cargo-zigbuild zig {subcommand} -- \"$@\"\n")
    }
}

fn write_wrapper(path: &Path, body: &str) -> Result<(), SoldrError> {
    let existing = std::fs::read_to_string(path).ok();
    if existing.as_deref() != Some(body) {
        std::fs::write(path, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_rust_targets_to_zig_targets() {
        assert_eq!(
            rust_target_to_zig_target("x86_64-unknown-linux-gnu").unwrap(),
            "x86_64-linux-gnu"
        );
        assert_eq!(
            rust_target_to_zig_target("aarch64-unknown-linux-musl").unwrap(),
            "aarch64-linux-musl"
        );
        assert_eq!(
            rust_target_to_zig_target("x86_64-apple-darwin").unwrap(),
            "x86_64-macos-none"
        );
    }

    #[test]
    fn rejects_unknown_targets() {
        assert!(matches!(
            rust_target_to_zig_target("wasm32-unknown-unknown"),
            Err(SoldrError::UnsupportedPlatform(_))
        ));
    }

    #[test]
    fn cc_wrapper_routes_through_zig_with_target() {
        if cfg!(windows) {
            return;
        }
        let body = render_cc_wrapper("cc", "aarch64-linux-musl");
        assert!(body.starts_with("#!/bin/sh\n"));
        assert!(body.contains("cargo-zigbuild zig cc -- -target aarch64-linux-musl"));
        assert!(body.contains("\"$@\""));
    }

    #[test]
    fn tool_wrapper_routes_through_cargo_zigbuild() {
        if cfg!(windows) {
            return;
        }
        let body = render_tool_wrapper("ranlib");
        assert!(body.contains("cargo-zigbuild zig ranlib --"));
        assert!(body.contains("\"$@\""));
    }
}
