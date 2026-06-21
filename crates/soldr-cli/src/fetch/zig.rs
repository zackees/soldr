//! Auto-bootstrap a `zig` binary for `cargo zigbuild` to consume.
//!
//! cargo-zigbuild shells out to a `zig` executable to do the cross-link
//! step. Until this module existed, `soldr cargo zigbuild ...` would
//! fetch `cargo-zigbuild` from `known_tools` but leave zig itself
//! missing — every cross-compile lane on a fresh runner exploded with
//! `Error: Failed to find zig / cannot find binary path` (observed in
//! `cross-compile-all-targets.yml` run 27893281530). The fix lives in
//! soldr, not the workflow, because soldr advertises itself as the
//! bootstrapper for cross builds (CLAUDE.md "Pre-built first") and
//! every consumer of `soldr cargo zigbuild` benefits.
//!
//! Resolution order, mirroring cargo-zigbuild's own logic:
//!   1. `ZIG` env var pointing at an existing file → use it.
//!   2. `zig` (or `zig.exe`) already on `PATH` → use it.
//!   3. Managed fetch from `https://ziglang.org/download/...`, cached
//!      at `~/.soldr/bin/zig-<MANAGED_ZIG_VERSION>/`.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

use super::github::http_client;
use super::trust;

/// Zig version that ships in soldr's managed bootstrap.
/// 0.13.0 is the long-tested LTS release of zig (June 2024) and is the
/// floor that cargo-zigbuild 0.20+ depends on. Bumping is a one-line
/// change here, but keep the major.minor stable between consumer
/// releases — cargo-zigbuild itself is sensitive to zig API breakage.
pub const MANAGED_ZIG_VERSION: &str = "0.13.0";

const ZIG_ENV_VAR: &str = "ZIG";

/// Ensure a zig binary is available for cargo-zigbuild. Returns the
/// **directory** holding the binary so the caller can prepend it to
/// `PATH` (matching how `ensure_known_subcommand_tool` already wires
/// `cargo-zigbuild` into the child cargo's environment).
pub async fn ensure_zig(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(dir) = zig_dir_from_env_var() {
        return Ok(dir);
    }
    if let Some(dir) = zig_dir_from_path() {
        return Ok(dir);
    }

    paths.ensure_dirs()?;

    let install_dir = paths.bin.join(format!("zig-{MANAGED_ZIG_VERSION}"));
    let stamp = install_dir.join(".complete");
    let cached_bin = managed_zig_binary_path(&install_dir);
    if stamp.is_file() && cached_bin.is_file() {
        if let Some(parent) = cached_bin.parent() {
            return Ok(parent.to_path_buf());
        }
    }

    let (asset, url) = zig_download_url(MANAGED_ZIG_VERSION)?;
    eprintln!("soldr: fetching zig v{MANAGED_ZIG_VERSION} ({asset})...");

    let client = http_client()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "zig download {url} failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    let digest = trust::sha256_of(&bytes);
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    match trust::verify_download("zig", MANAGED_ZIG_VERSION, &asset, &digest, &store, mode)? {
        trust::VerifyOutcome::Verified { sha256 } => {
            eprintln!("soldr: trust: verified zig v{MANAGED_ZIG_VERSION} {asset} sha256={sha256}");
        }
        trust::VerifyOutcome::Unverified { sha256 } => {
            eprintln!(
                "soldr: trust: unverified zig v{MANAGED_ZIG_VERSION} {asset} sha256={sha256} (set {} to pin; run with {}=strict to require pins)",
                trust::CHECKSUMS_FILE_ENV_VAR,
                trust::TRUST_MODE_ENV_VAR
            );
        }
    }

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    std::fs::create_dir_all(&install_dir)?;

    if asset.ends_with(".zip") {
        extract_zip_tree(&bytes, &install_dir)?;
    } else {
        extract_tar_xz_tree(&bytes, &install_dir)?;
    }

    let resolved = managed_zig_binary_path(&install_dir);
    if !resolved.is_file() {
        return Err(SoldrError::Archive(format!(
            "zig binary not found after extract at {}",
            resolved.display()
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&resolved)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&resolved, perms)?;
    }

    std::fs::write(&stamp, MANAGED_ZIG_VERSION)?;
    eprintln!("soldr: downloaded zig v{MANAGED_ZIG_VERSION}");

    resolved
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SoldrError::Other("zig binary has no parent directory".into()))
}

fn zig_dir_from_env_var() -> Option<PathBuf> {
    let value = std::env::var_os(ZIG_ENV_VAR)?;
    let p = PathBuf::from(value);
    if p.is_file() {
        p.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn zig_dir_from_path() -> Option<PathBuf> {
    let exe = zig_binary_filename();
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(dir);
        }
    }
    None
}

fn zig_binary_filename() -> &'static str {
    if cfg!(windows) {
        "zig.exe"
    } else {
        "zig"
    }
}

fn managed_zig_binary_path(install_dir: &Path) -> PathBuf {
    install_dir
        .join(managed_zig_archive_root())
        .join(zig_binary_filename())
}

/// The official zig tarballs / zips unpack to a single top-level
/// directory `zig-<os>-<arch>-<version>` (the 0.13.x naming
/// convention). Mirror that so we know exactly where to find the
/// extracted binary without scanning.
fn managed_zig_archive_root() -> String {
    let (os, arch) = host_zig_os_arch().unwrap_or(("linux", "x86_64"));
    format!("zig-{os}-{arch}-{MANAGED_ZIG_VERSION}")
}

fn host_zig_os_arch() -> Option<(&'static str, &'static str)> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        return None;
    };
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        return None;
    };
    Some((os, arch))
}

fn zig_download_url(version: &str) -> Result<(String, String), SoldrError> {
    let Some((os, arch)) = host_zig_os_arch() else {
        return Err(SoldrError::Other(format!(
            "unsupported host for zig bootstrap: arch={} os={}",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )));
    };
    let ext = if os == "windows" { "zip" } else { "tar.xz" };
    let asset = format!("zig-{os}-{arch}-{version}.{ext}");
    let url = format!("https://ziglang.org/download/{version}/{asset}");
    Ok((asset, url))
}

fn extract_tar_xz_tree(data: &[u8], dest: &Path) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let xz = xz2::read::XzDecoder::new(reader);
    let mut archive = tar::Archive::new(xz);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

fn extract_zip_tree(data: &[u8], dest: &Path) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;
    archive
        .extract(dest)
        .map_err(|e| SoldrError::Archive(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(zig_download_url_known_targets, {
        // Smoke: we don't hit the network in unit tests, just verify the
        // URL builder formats the asset name as `zig-<os>-<arch>-<ver>.<ext>`.
        let (asset, url) =
            zig_download_url("0.13.0").expect("host should resolve in unit-test env");
        assert!(
            asset.starts_with("zig-") && asset.contains("-0.13.0."),
            "asset name should embed version: {asset}",
        );
        assert!(
            url.starts_with("https://ziglang.org/download/0.13.0/"),
            "url should target ziglang.org/download/<ver>/: {url}",
        );
        assert!(
            asset.ends_with(".tar.xz") || asset.ends_with(".zip"),
            "asset extension should match the OS family: {asset}",
        );
    });

    crate::timed_test!(env_var_overrides_path_and_managed_install, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let exe = tmp.path().join(zig_binary_filename());
        std::fs::write(&exe, b"#!/bin/sh\necho fake-zig\n").expect("write");
        let prev = std::env::var_os(ZIG_ENV_VAR);
        std::env::set_var(ZIG_ENV_VAR, &exe);
        let resolved = zig_dir_from_env_var();
        match prev {
            Some(v) => std::env::set_var(ZIG_ENV_VAR, v),
            None => std::env::remove_var(ZIG_ENV_VAR),
        }
        assert_eq!(resolved.as_deref(), exe.parent());
    });
}
