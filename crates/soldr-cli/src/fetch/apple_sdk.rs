//! Auto-bootstrap the Apple macOS SDK for `cargo zigbuild --target
//! *-apple-darwin` cross-compile.
//!
//! Without an Apple SDK on disk + `SDKROOT` exported, cargo-zigbuild's
//! mach-O linker fails on every Rust dep that links `-framework IOKit`
//! / `-framework CoreFoundation` etc. (ring, sysinfo, dirs, …):
//!
//!     error: unable to find framework 'IOKit'. searched paths:  none
//!
//! Mirrors PR #841's `ensure_zig` pattern: soldr is the bootstrapper
//! (CLAUDE.md "Pre-built first"), so every consumer of `soldr cargo
//! zigbuild --target *-apple-darwin` Just Works on a stock device.
//!
//! Resolution order:
//!   1. `SDKROOT` env var pointing at an existing dir → use it (escape
//!      hatch for Xcode users on macOS hosts + advanced users with a
//!      custom SDK).
//!   2. `xcrun --show-sdk-path` (macOS host only, with Xcode installed)
//!      → use what Apple's tooling points at.
//!   3. Managed fetch from the soldr `manifest` branch
//!      (`deps/mac/sdk.tar.zstd`, ~52 MiB compressed), cached at
//!      `~/.soldr/sdk/MacOSX11.3.sdk/`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::core::{SoldrError, SoldrPaths};

use super::github::http_client;
use super::trust;

/// Pinned macOS SDK version that soldr's managed bootstrap ships.
/// 11.3 is the version extracted from `messense/cargo-zigbuild:0.20.0`
/// (the docker image the cross-compile workflow used before this
/// auto-bootstrap landed). Building for `mmacosx-version-min=10.12.0`
/// or newer works fine with this SDK.
pub const MANAGED_APPLE_SDK_VERSION: &str = "11.3";

/// Directory basename of the extracted SDK (matches the upstream
/// tarball's top-level directory, which is what cargo-zigbuild's auto
/// discovery expects).
pub const MANAGED_APPLE_SDK_DIRNAME: &str = "MacOSX11.3.sdk";

/// URL of the SDK blob on soldr's `manifest` branch. Uses
/// `media.githubusercontent.com/media/` — the LFS-aware CDN endpoint
/// that follows LFS pointer files to the actual binary content (works
/// for both LFS-tracked and regular blobs, matching the pattern from
/// `zackees/clang-tool-chain-bins`). NOT subject to the GitHub API
/// rate limit. The blob was extracted once from
/// `messense/cargo-zigbuild:0.20.0` and pushed under `deps/mac/sdk.tar.zstd`.
pub const MANAGED_APPLE_SDK_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr/manifest/deps/mac/sdk.tar.zstd";

/// SHA-256 of the SDK blob. Hard-coded for integrity verification on
/// every fetch. Bump alongside `MANAGED_APPLE_SDK_VERSION` when the
/// blob is regenerated.
pub const MANAGED_APPLE_SDK_SHA256: &str =
    "053ac5617f5e6afd5218bec4e871cc55a6a9ab2c0b1f2f77e336dbdd48eabe56";

const SDKROOT_ENV_VAR: &str = "SDKROOT";

/// Ensure an Apple macOS SDK is available for cargo-zigbuild's
/// mach-O linker. Returns the **path of the SDK directory** (the
/// `*.sdk` dir, not its parent) so the caller can set `SDKROOT` to
/// it on the child cargo invocation.
pub async fn ensure_apple_sdk(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(sdk) = sdk_from_env_var() {
        return Ok(sdk);
    }
    if let Some(sdk) = sdk_from_xcrun() {
        return Ok(sdk);
    }
    fetch_managed_sdk(paths).await
}

fn sdk_from_env_var() -> Option<PathBuf> {
    let value = std::env::var_os(SDKROOT_ENV_VAR)?;
    let p = PathBuf::from(value);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// On macOS hosts with Xcode installed, `xcrun --show-sdk-path` returns
/// the SDK path Apple's tooling expects. Use it when present so we
/// don't redundantly fetch our own copy on developer macs.
fn sdk_from_xcrun() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path_str = String::from_utf8(out.stdout).ok()?;
    let p = PathBuf::from(path_str.trim());
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

async fn fetch_managed_sdk(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;
    let install_dir = paths.bin.join("apple-sdk").join(MANAGED_APPLE_SDK_VERSION);
    let stamp = install_dir.join(".complete");
    let sdk_dir = install_dir.join(MANAGED_APPLE_SDK_DIRNAME);

    if stamp.is_file() && sdk_dir.is_dir() {
        return Ok(sdk_dir);
    }

    eprintln!(
        "soldr: fetching Apple SDK v{MANAGED_APPLE_SDK_VERSION} from {MANAGED_APPLE_SDK_URL}..."
    );

    let client = http_client()?;
    let resp = client
        .get(MANAGED_APPLE_SDK_URL)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "Apple SDK download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    // Integrity check is mandatory — the blob is hand-curated and the
    // sha256 is pinned in this file. A mismatch means either the
    // manifest branch was tampered with or the constants drifted; refuse
    // to extract in either case.
    let digest = trust::sha256_of(&bytes);
    if digest != MANAGED_APPLE_SDK_SHA256 {
        return Err(SoldrError::Other(format!(
            "Apple SDK sha256 mismatch: expected {MANAGED_APPLE_SDK_SHA256}, got {digest} \
             (manifest branch blob may have been replaced — refusing to extract)"
        )));
    }
    eprintln!("soldr: trust: verified Apple SDK v{MANAGED_APPLE_SDK_VERSION} sha256={digest}");

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    std::fs::create_dir_all(&install_dir)?;
    extract_tar_zst_tree(&bytes, &install_dir)?;

    if !sdk_dir.is_dir() {
        return Err(SoldrError::Archive(format!(
            "Apple SDK extract did not produce expected directory {}",
            sdk_dir.display()
        )));
    }

    std::fs::write(&stamp, MANAGED_APPLE_SDK_VERSION)?;
    eprintln!("soldr: extracted Apple SDK to {}", sdk_dir.display());
    Ok(sdk_dir)
}

fn extract_tar_zst_tree(data: &[u8], dest: &Path) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(env_var_overrides_when_pointing_at_real_dir, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let fake_sdk = tmp.path().join("FakeMacOSX.sdk");
        std::fs::create_dir_all(&fake_sdk).expect("mk");
        let prev = std::env::var_os(SDKROOT_ENV_VAR);
        std::env::set_var(SDKROOT_ENV_VAR, &fake_sdk);
        let resolved = sdk_from_env_var();
        match prev {
            Some(v) => std::env::set_var(SDKROOT_ENV_VAR, v),
            None => std::env::remove_var(SDKROOT_ENV_VAR),
        }
        assert_eq!(resolved.as_deref(), Some(fake_sdk.as_path()));
    });

    crate::timed_test!(env_var_ignored_when_path_is_missing, {
        let prev = std::env::var_os(SDKROOT_ENV_VAR);
        std::env::set_var(SDKROOT_ENV_VAR, "/definitely/not/a/real/path/3819237");
        let resolved = sdk_from_env_var();
        match prev {
            Some(v) => std::env::set_var(SDKROOT_ENV_VAR, v),
            None => std::env::remove_var(SDKROOT_ENV_VAR),
        }
        assert!(
            resolved.is_none(),
            "missing dir should be ignored: {resolved:?}"
        );
    });

    crate::timed_test!(constants_are_well_formed, {
        // Smoke: catch typos in the URL / sha256 / dir name during refactor.
        assert!(MANAGED_APPLE_SDK_URL.starts_with("https://"));
        assert!(
            MANAGED_APPLE_SDK_URL.ends_with(".tar.zst")
                || MANAGED_APPLE_SDK_URL.ends_with(".tar.zstd")
        );
        assert_eq!(MANAGED_APPLE_SDK_SHA256.len(), 64);
        assert!(MANAGED_APPLE_SDK_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert!(MANAGED_APPLE_SDK_DIRNAME.ends_with(".sdk"));
    });
}
