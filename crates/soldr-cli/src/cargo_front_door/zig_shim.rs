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

    let ext = crate::platform::executable::name::script_suffix();
    let cc = dir.join(format!("cc{ext}"));
    let cxx = dir.join(format!("cxx{ext}"));
    let ar = dir.join(format!("ar{ext}"));
    let ranlib = dir.join(format!("ranlib{ext}"));

    let is_darwin = triple.ends_with("-apple-darwin");
    write_wrapper(&cc, &render_cc_wrapper("cc", zig_target, is_darwin))?;
    write_wrapper(&cxx, &render_cc_wrapper("c++", zig_target, is_darwin))?;
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
        "x86_64-pc-windows-gnu" => Ok("x86_64-windows-gnu"),
        "x86_64-apple-darwin" => Ok("x86_64-macos-none"),
        "aarch64-apple-darwin" => Ok("aarch64-macos-none"),
        _ => Err(SoldrError::UnsupportedPlatform(format!(
            "soldr cargo zigbuild env bootstrap does not support target `{triple}`"
        ))),
    }
}

fn render_cc_wrapper(subcommand: &str, zig_target: &str, is_darwin: bool) -> String {
    if is_darwin {
        return render_darwin_cc_wrapper(subcommand, zig_target);
    }
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!("@echo off\r\ncargo-zigbuild zig {subcommand} -- -target {zig_target} %*\r\n",)
    } else {
        format!("#!/bin/sh\nexec cargo-zigbuild zig {subcommand} -- -target {zig_target} \"$@\"\n",)
    }
}

/// Darwin cc/c++ wrapper — sidesteps cargo-zigbuild's broken
/// `--sysroot` handling on zig < 0.15 (darwin lanes of run 28574600982).
///
/// When `SDKROOT` is set, cargo-zigbuild's `zig cc` proxy (v0.23.0,
/// zig < 0.15 branch of `add_macos_specific_args`) passes BOTH
/// `--sysroot=$SDKROOT` and absolute `-L$SDKROOT/usr/lib` /
/// `-F$SDKROOT/System/Library/Frameworks` args. zig 0.14's Mach-O
/// linker interprets every `-L` path as *relative to the sysroot*, so
/// each search dir doubles up (`$SDKROOT$SDKROOT/usr/lib`,
/// `$SDKROOT/<build-out-dir>`, …) and no system library resolves:
///
///   error: unable to find dynamic system library 'objc'
///           using strategy 'paths_first'. searched paths: none
///
/// cargo-zigbuild's own fix only engages on zig >= 0.15 (SDKROOT env
/// var instead of `--sysroot`). Since soldr pins zig 0.14.1, the shim
/// clears `SDKROOT` before exec'ing cargo-zigbuild (so it never adds
/// `--sysroot`) and passes the SDK include/library/framework search
/// paths explicitly — the exact arg shape cargo-zigbuild's zig >= 0.15
/// branch would produce. Non-darwin targets keep the plain wrapper.
fn render_darwin_cc_wrapper(subcommand: &str, zig_target: &str) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!(
            "@echo off\r\n\
             setlocal\r\n\
             set \"SOLDR_APPLE_SDK=%SDKROOT%\"\r\n\
             set \"SDKROOT=\"\r\n\
             if defined SOLDR_APPLE_SDK (\r\n\
               cargo-zigbuild zig {subcommand} -- -target {zig_target} \
             -isystem \"%SOLDR_APPLE_SDK%\\usr\\include\" \
             \"-L%SOLDR_APPLE_SDK%\\usr\\lib\" \
             \"-F%SOLDR_APPLE_SDK%\\System\\Library\\Frameworks\" \
             -iframework \"%SOLDR_APPLE_SDK%\\System\\Library\\Frameworks\" \
             -DTARGET_OS_IPHONE=0 %*\r\n\
             ) else (\r\n\
               cargo-zigbuild zig {subcommand} -- -target {zig_target} %*\r\n\
             )\r\n"
        )
    } else {
        format!(
            "#!/bin/sh\n\
             # soldr zig shim (darwin): see zig_shim.rs — clears SDKROOT so\n\
             # cargo-zigbuild (zig < 0.15) does not emit --sysroot, which\n\
             # makes zig resolve every -L path relative to the SDK. The SDK\n\
             # search paths are passed explicitly instead.\n\
             if [ -n \"${{SDKROOT:-}}\" ]; then\n\
               SOLDR_APPLE_SDK=\"$SDKROOT\"\n\
               unset SDKROOT\n\
               exec cargo-zigbuild zig {subcommand} -- -target {zig_target} \\\n\
                 -isystem \"$SOLDR_APPLE_SDK/usr/include\" \\\n\
                 \"-L$SOLDR_APPLE_SDK/usr/lib\" \\\n\
                 \"-F$SOLDR_APPLE_SDK/System/Library/Frameworks\" \\\n\
                 -iframework \"$SOLDR_APPLE_SDK/System/Library/Frameworks\" \\\n\
                 -DTARGET_OS_IPHONE=0 \"$@\"\n\
             fi\n\
             exec cargo-zigbuild zig {subcommand} -- -target {zig_target} \"$@\"\n"
        )
    }
}

fn render_tool_wrapper(subcommand: &str) -> String {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        format!("@echo off\r\ncargo-zigbuild zig {subcommand} -- %*\r\n")
    } else {
        format!("#!/bin/sh\nexec cargo-zigbuild zig {subcommand} -- \"$@\"\n")
    }
}

fn write_wrapper(path: &Path, body: &str) -> Result<(), SoldrError> {
    let existing = std::fs::read_to_string(path).ok();
    if existing.as_deref() != Some(body) {
        std::fs::write(path, body)?;
        crate::platform::fs::permissions::make_executable(path).map_err(SoldrError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(maps_supported_rust_targets_to_zig_targets, {
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
        assert_eq!(
            rust_target_to_zig_target("x86_64-pc-windows-gnu").unwrap(),
            "x86_64-windows-gnu"
        );
    });

    crate::timed_test!(rejects_unknown_targets, {
        assert!(matches!(
            rust_target_to_zig_target("wasm32-unknown-unknown"),
            Err(SoldrError::UnsupportedPlatform(_))
        ));
    });

    crate::timed_test!(cc_wrapper_routes_through_zig_with_target, {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let body = render_cc_wrapper("cc", "aarch64-linux-musl", false);
        assert!(body.starts_with("#!/bin/sh\n"));
        assert!(body.contains("cargo-zigbuild zig cc -- -target aarch64-linux-musl"));
        assert!(body.contains("\"$@\""));
        // Non-darwin wrappers must NOT carry the SDKROOT workaround.
        assert!(!body.contains("SDKROOT"));
    });

    crate::timed_test!(
        darwin_cc_wrapper_clears_sdkroot_and_adds_explicit_sdk_paths,
        {
            if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
                return;
            }
            // Run 28574600982: with SDKROOT set, cargo-zigbuild (zig < 0.15)
            // passes `--sysroot=$SDKROOT` and zig 0.14 then resolves every
            // -L path relative to the sysroot — no system library resolves.
            // The darwin wrapper must clear SDKROOT before exec'ing
            // cargo-zigbuild and pass the SDK search paths explicitly.
            let body = render_cc_wrapper("cc", "x86_64-macos-none", true);
            assert!(body.starts_with("#!/bin/sh\n"));
            assert!(body.contains("unset SDKROOT"));
            assert!(body.contains("-L$SOLDR_APPLE_SDK/usr/lib"));
            assert!(body.contains("-F$SOLDR_APPLE_SDK/System/Library/Frameworks"));
            assert!(body.contains("-isystem \"$SOLDR_APPLE_SDK/usr/include\""));
            assert!(body.contains("cargo-zigbuild zig cc -- -target x86_64-macos-none"));
            assert!(body.contains("\"$@\""));
            // Fallback branch (no SDKROOT in env) keeps the plain invocation.
            assert!(
                body.ends_with("exec cargo-zigbuild zig cc -- -target x86_64-macos-none \"$@\"\n")
            );
        }
    );

    crate::timed_test!(tool_wrapper_routes_through_cargo_zigbuild, {
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let body = render_tool_wrapper("ranlib");
        assert!(body.contains("cargo-zigbuild zig ranlib --"));
        assert!(body.contains("\"$@\""));
    });
}
