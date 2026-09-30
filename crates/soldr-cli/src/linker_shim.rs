//! Content-addressed Linux clang driver shims, and reld path injection.
//!
//! Split out of `linker.rs` to stay under the per-file line ceiling
//! (soldr#3276): this module has one cohesive job -- materializing a
//! driver-only executable shim so a Linux clang driver argument never enters
//! Cargo's `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, which would silently replace a
//! project's own `[build] rustflags` (soldr#3277) -- plus the reld-specific
//! path substitution that runs just before a shim is materialized.

use crate::core::SoldrError;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::linker::{target_kind, LinkerInjection, TargetKind};

const LINKER_SHIM_FORMAT_VERSION: &str = "v1";
static LINKER_SHIM_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace the bare `reld` token in an explicit linker injection with a
/// verified absolute executable path resolved before Cargo starts.
pub fn inject_resolved_reld(
    injection: &mut LinkerInjection,
    reld: &Path,
) -> Result<(), SoldrError> {
    let reld = reld.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "managed reld path is not valid UTF-8: {}",
            reld.display()
        ))
    })?;
    let mut replaced = false;
    if injection.linker.as_deref() == Some("reld") {
        injection.linker = Some(reld.to_string());
        replaced = true;
    }
    if let Some(flags) = injection.rustflags.as_mut() {
        if flags.contains("--ld-path=reld") {
            *flags = flags.replacen("--ld-path=reld", &format!("--ld-path={reld}"), 1);
            replaced = true;
        }
    }
    if replaced {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "explicit reld linker selection produced no reld injection".to_string(),
        ))
    }
}

/// Move a Linux or Apple clang driver argument out of target-scoped rustflags and into
/// a content-addressed executable shim.
///
/// Cargo does not merge `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` with
/// `[build] rustflags`; the environment value replaces the project setting.
/// A linker shim carries the driver-only argument without entering Cargo's
/// rustflags precedence at all. Its content digest is embedded in the path so
/// the existing `CARGO_TARGET_*_LINKER` cache-key input changes whenever the
/// generated command changes.
pub fn materialize_linker_driver_shim(
    paths: &crate::core::SoldrPaths,
    target: &str,
    injection: &mut LinkerInjection,
    clang: Option<&Path>,
) -> Result<(), SoldrError> {
    let Some(driver_arg) = shim_driver_arg(target, injection) else {
        return Ok(());
    };
    let driver_arg = driver_arg.to_string();

    let body = render_linker_driver_shim(clang, &driver_arg)?;
    let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
    let dir = paths
        .bin
        .join("linker-shims")
        .join(LINKER_SHIM_FORMAT_VERSION);
    std::fs::create_dir_all(&dir)?;
    let suffix = crate::platform::executable::name::script_suffix();
    let path = dir.join(format!("clang-{}{suffix}", &digest[..20]));
    write_content_addressed_shim(&path, &body)?;

    injection.linker = Some(path.to_string_lossy().into_owned());
    injection.rustflags = None;
    Ok(())
}

/// The clang driver argument that a shim would carry, or `None` when this
/// target/injection needs no shim.
fn shim_driver_arg<'a>(target: &str, injection: &'a LinkerInjection) -> Option<&'a str> {
    if !matches!(target_kind(target), TargetKind::Linux | TargetKind::Apple)
        || injection.linker.as_deref() != Some("clang")
    {
        return None;
    }
    injection.rustflags.as_deref()?.strip_prefix("-C link-arg=")
}

/// [`materialize_linker_driver_shim`] after resolving the `clang` it will
/// exec (soldr#3430): a managed LLVM already on disk, else the `clang` on the
/// search path, else the catalogued LLVM fetched just in time. The shim names
/// that absolute path, so a host without a system clang gets one clear error
/// from soldr here rather than `exec: clang: not found` (exit 127) from some
/// dependency's build script.
pub async fn materialize_linker_driver_shim_resolving(
    paths: &crate::core::SoldrPaths,
    target: &str,
    injection: &mut LinkerInjection,
) -> Result<(), SoldrError> {
    if shim_driver_arg(target, injection).is_none() || windows_host() {
        return materialize_linker_driver_shim(paths, target, injection, None);
    }
    let clang = resolve_driver_clang(paths).await?;
    materialize_linker_driver_shim(paths, target, injection, Some(&clang))
}

fn windows_host() -> bool {
    crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
}

async fn resolve_driver_clang(paths: &crate::core::SoldrPaths) -> Result<PathBuf, SoldrError> {
    let managed_complete = paths
        .bin
        .join(format!("llvm-{}", crate::fetch::MANAGED_LLVM_VERSION))
        .join(".complete");
    if managed_complete.is_file() || std::env::var_os("SOLDR_LLVM_DIR").is_some() {
        if let Ok(bin) = crate::fetch::ensure_llvm_toolchain(paths).await {
            if let Some(clang) = clang_in(&bin) {
                return Ok(clang);
            }
        }
    }
    if let Some(search) = std::env::var_os("PATH") {
        if let Some(clang) = crate::exec_cmd::find_on_path("clang", &search) {
            return Ok(clang);
        }
    }
    let fetched = crate::fetch::ensure_llvm_toolchain(paths)
        .await
        .map_err(|error| missing_clang_error(&error.to_string()))?;
    clang_in(&fetched).ok_or_else(|| {
        missing_clang_error(&format!(
            "no clang in the fetched LLVM at {}",
            fetched.display()
        ))
    })
}

fn clang_in(bin_dir: &Path) -> Option<PathBuf> {
    let clang = bin_dir.join(format!("clang{}", std::env::consts::EXE_SUFFIX));
    clang.is_file().then_some(clang)
}

fn missing_clang_error(cause: &str) -> SoldrError {
    SoldrError::Other(missing_clang_error_text(cause))
}

pub(crate) fn missing_clang_error_text(cause: &str) -> String {
    format!(
        "the linker driver needs `clang`, but none is on the search path and soldr could not \
         provide one (managed LLVM v{}, catalogue asset llvm-{}): {cause}. Install clang, or \
         set SOLDR_LLVM_DIR to an existing LLVM bin directory.",
        crate::fetch::MANAGED_LLVM_VERSION,
        crate::fetch::MANAGED_LLVM_VERSION,
    )
}

/// The shim body. On Unix it execs an absolute `clang`; a bare `exec clang`
/// is never emitted, so a later search-path change cannot break it and the
/// content digest covers the real driver.
fn render_linker_driver_shim(clang: Option<&Path>, driver_arg: &str) -> Result<String, SoldrError> {
    if windows_host() {
        return Ok(render_windows_linker_driver_shim(driver_arg));
    }
    let clang = clang.ok_or_else(|| {
        SoldrError::Other("internal: linker driver shim rendered without a resolved clang".into())
    })?;
    let clang = clang.to_str().ok_or_else(|| {
        SoldrError::Other(format!(
            "clang path is not valid UTF-8: {}",
            clang.display()
        ))
    })?;
    Ok(format!(
        "#!/bin/sh\n# generated by soldr; content-addressed linker driver\nexec {} {} \"$@\"\n",
        shell_single_quote(clang),
        shell_single_quote(driver_arg)
    ))
}

pub(crate) fn render_windows_linker_driver_shim(driver_arg: &str) -> String {
    // The generated values are one driver argument and cannot contain a
    // double quote (Windows paths cannot either). Quoting the whole argument
    // protects spaces and cmd metacharacters; doubling percent signs prevents
    // environment-variable expansion before clang receives the value.
    let escaped = driver_arg.replace('%', "%%");
    format!("@echo off\r\nclang \"{escaped}\" %*\r\n")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn write_content_addressed_shim(path: &Path, body: &str) -> Result<(), SoldrError> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(body) {
        return Ok(());
    }
    let sequence = LINKER_SHIM_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    std::fs::write(&temp, body)?;
    crate::platform::fs::permissions::make_executable(&temp).map_err(SoldrError::Io)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(_) if std::fs::read_to_string(path).ok().as_deref() == Some(body) => {
            let _ = std::fs::remove_file(&temp);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(SoldrError::Io(error))
        }
    }
}
