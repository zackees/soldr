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
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::{SoldrError, SoldrPaths};

use super::trust;

/// Zig version that ships in soldr's managed bootstrap.
///
/// 0.14.1 (March 2025) is the floor that picks up cargo-zigbuild's
/// macOS SDKROOT fixes (v0.21.2 — rust-cross/cargo-zigbuild#387) and
/// the Xcode 15 / Apple SDK linker work that resolves the
/// `unable to find dynamic system library 'objc'` failure observed
/// on soldr's CI Apple cross-build lanes. cargo-zigbuild v0.21.4
/// added zig-0.15 compat and v0.23.0 handles zig-0.15 ZON output; we
/// stay on 0.14.1 because the API churn between zig 0.14 and 0.15 is
/// material and 0.14.1 is the most-tested combination with
/// cargo-zigbuild's current release. Bump in lockstep with cargo-
/// zigbuild whenever zig 0.15+ becomes the floor downstream needs.
pub const MANAGED_ZIG_VERSION: &str = "0.14.1";

const ZIG_ENV_VAR: &str = "ZIG";
const ZIG_DOWNLOAD_ATTEMPTS: u32 = 4;
const ZIG_DOWNLOAD_INITIAL_BACKOFF: Duration = Duration::from_secs(5);

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

    let bytes = download_zig_asset(&url).await?;

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

async fn download_zig_asset(url: &str) -> Result<Vec<u8>, SoldrError> {
    let client = zig_http_client()?;
    let mut attempt = 1;
    let mut backoff = ZIG_DOWNLOAD_INITIAL_BACKOFF;
    loop {
        let result = download_zig_asset_once(&client, url).await;
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(err) if attempt < ZIG_DOWNLOAD_ATTEMPTS => {
                eprintln!(
                    "soldr: transient error fetching zig (attempt {attempt}/{ZIG_DOWNLOAD_ATTEMPTS}): {err}; retrying in {:?}",
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
                attempt += 1;
            }
            Err(err) => match download_zig_asset_with_curl(url) {
                Ok(bytes) => return Ok(bytes),
                Err(curl_err) => {
                    return Err(SoldrError::Network(format!(
                        "{err}; curl fallback also failed: {curl_err}"
                    )));
                }
            },
        }
    }
}

fn zig_http_client() -> Result<reqwest::Client, SoldrError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .http1_only()
        .user_agent(format!("soldr/{}", crate::core::version()))
        .build()
        .map_err(|e| SoldrError::Network(e.to_string()))
}

async fn download_zig_asset_once(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<u8>, SoldrError> {
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
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
    Ok(bytes.to_vec())
}

fn download_zig_asset_with_curl(url: &str) -> Result<Vec<u8>, SoldrError> {
    eprintln!("soldr: falling back to curl for zig download...");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    // soldr#1900: the zig tarball is tens of megabytes. `std::env::temp_dir()`
    // is tmpfs on most Linux hosts, so this used to be downloaded straight
    // into RAM and then read into a Vec on top of that.
    let out = soldr_core::core::ensure_temp_root().join(format!(
        "soldr-zig-download-{}-{nonce}.tmp",
        std::process::id()
    ));
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "10",
            "--max-time",
            "600",
            "--retry",
            "6",
            "--retry-delay",
            "5",
            "--retry-all-errors",
            "--output",
        ])
        .arg(&out)
        .arg(url)
        .status()
        .map_err(|err| SoldrError::Network(format!("failed to invoke curl: {err}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(SoldrError::Network(format!(
            "curl exited with status {status}"
        )));
    }
    let bytes = std::fs::read(&out).map_err(|err| {
        SoldrError::Network(format!(
            "failed to read curl download {}: {err}",
            out.display()
        ))
    });
    let _ = std::fs::remove_file(&out);
    bytes
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
    // Try the constructed name first (no I/O). Fall back to a single-
    // depth scan if the constructed name doesn't exist. The scan is
    // the robustness layer for future upstream naming changes
    // (see soldr#1032 — the zig-0.14.0 asset-naming swap also flipped
    // the tarball's internal top-level dir name, and the previous
    // hardcoded `zig-{os}-{arch}-{ver}` formula in `managed_zig_archive_root`
    // pointed at a directory that doesn't exist for 0.14+).
    let constructed = install_dir
        .join(managed_zig_archive_root())
        .join(zig_binary_filename());
    if constructed.is_file() {
        return constructed;
    }
    if let Some(scanned) = scan_for_zig_binary(install_dir) {
        return scanned;
    }
    constructed
}

/// The official zig tarballs unpack to a single top-level directory
/// `zig-<arch>-<os>-<version>` (the 0.14+ naming) or
/// `zig-<os>-<arch>-<version>` (the 0.13.x naming). Match the same
/// branching used by `zig_download_url` so the extracted directory
/// name is found on the first try without scanning.
fn managed_zig_archive_root() -> String {
    let (os, arch) = host_zig_os_arch().unwrap_or(("linux", "x86_64"));
    let pre_0_14 = matches!(
        MANAGED_ZIG_VERSION,
        "0.13.0" | "0.12.0" | "0.11.0" | "0.10.1" | "0.10.0"
    );
    if pre_0_14 {
        format!("zig-{os}-{arch}-{MANAGED_ZIG_VERSION}")
    } else {
        format!("zig-{arch}-{os}-{MANAGED_ZIG_VERSION}")
    }
}

/// Fallback: look one level down from `install_dir` for a directory
/// that contains the zig binary. Robustness layer if upstream changes
/// the top-level directory naming again in a future release without
/// soldr getting a coordinated bump.
fn scan_for_zig_binary(install_dir: &Path) -> Option<PathBuf> {
    let exe = zig_binary_filename();
    let read_dir = std::fs::read_dir(install_dir).ok()?;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
    // Asset-naming swap landed in zig 0.14.0: pre-0.14 used
    // `zig-{os}-{arch}-{ver}.tar.xz`; 0.14+ uses
    // `zig-{arch}-{os}-{ver}.tar.xz`. Branch on the major/minor here
    // rather than threading a per-version table — the swap is the
    // only naming change in the version range soldr cares about.
    let pre_0_14 = matches!(
        version,
        "0.13.0" | "0.12.0" | "0.11.0" | "0.10.1" | "0.10.0"
    );
    let asset = if pre_0_14 {
        format!("zig-{os}-{arch}-{version}.{ext}")
    } else {
        format!("zig-{arch}-{os}-{version}.{ext}")
    };
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
        // URL builder formats the asset name correctly.
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

    crate::timed_test!(zig_download_url_naming_swap_at_0_14, {
        // Zig 0.13.0: pre-swap naming → `zig-{os}-{arch}-{ver}.tar.xz`.
        // Zig 0.14.0+: post-swap naming → `zig-{arch}-{os}-{ver}.tar.xz`.
        let (asset_13, _) = zig_download_url("0.13.0").unwrap();
        let (asset_14, _) = zig_download_url("0.14.1").unwrap();
        // Whatever host the unit tests run on, the arch + os tokens
        // must appear in swapped order between the two versions.
        let Some((os, arch)) = host_zig_os_arch() else {
            return;
        };
        let pre_13 = format!("zig-{os}-{arch}-0.13.0");
        let post_14 = format!("zig-{arch}-{os}-0.14.1");
        assert!(asset_13.starts_with(&pre_13), "0.13.0: {asset_13}");
        assert!(asset_14.starts_with(&post_14), "0.14.1: {asset_14}");
    });

    crate::timed_test!(managed_zig_archive_root_swaps_with_version, {
        // The directory name inside the tarball mirrors the asset name.
        // 0.13.x uses `zig-{os}-{arch}-{ver}/`, 0.14+ uses
        // `zig-{arch}-{os}-{ver}/`. This test confirms managed_zig_archive_root
        // follows the same pre/post-0.14 branching as zig_download_url —
        // they MUST agree or extracts land in a directory the lookup misses
        // (soldr#1032).
        let Some((os, arch)) = host_zig_os_arch() else {
            return;
        };
        let root = managed_zig_archive_root();
        // MANAGED_ZIG_VERSION is currently 0.14.1 (post-swap).
        let expected = format!("zig-{arch}-{os}-{MANAGED_ZIG_VERSION}");
        assert_eq!(
            root, expected,
            "managed_zig_archive_root should match the 0.14+ post-swap layout"
        );
    });

    crate::timed_test!(managed_zig_binary_path_falls_back_to_scan, {
        // Even if upstream changes the directory naming AGAIN in a future
        // release without soldr getting a coordinated bump, the scan
        // fallback should locate the binary. soldr#1032 hardening.
        let tmp = tempfile::tempdir().expect("tmpdir");
        // Create a directory whose name does NOT match the constructed
        // one, containing the zig binary.
        let surprise = tmp.path().join("zig-some-weird-future-naming-0.99.0");
        std::fs::create_dir_all(&surprise).unwrap();
        let zig = surprise.join(zig_binary_filename());
        std::fs::write(&zig, b"#!/bin/sh\necho fake\n").unwrap();
        let resolved = managed_zig_binary_path(tmp.path());
        assert_eq!(
            resolved, zig,
            "scan fallback should find the zig binary in any subdirectory"
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
