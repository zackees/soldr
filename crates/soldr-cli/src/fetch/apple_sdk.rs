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

/// soldr-toolchain#14 Phase 6 — env var that pins the Apple SDK
/// version soldr fetches. Values: `"11.3" | "13.3" | "14.5" | "15.2"`.
/// Unset → falls back to [`MANAGED_APPLE_SDK_VERSION`] (the boring
/// default for compat).
pub const APPLE_SDK_VERSION_ENV_VAR: &str = "SOLDR_APPLE_SDK_VERSION";

/// soldr-toolchain#14 Phase 6 — env var that pins the Apple SDK
/// *shape*. Values: `"universal2" | "thin-x86_64" | "thin-aarch64" |
/// "auto"`. Unset → `auto` (use the host arch's thin variant when
/// cross-compiling for one target arch, universal2 when targeting
/// both). The "auto" knob defaults to `universal2` until the catalogue
/// is populated for thin shapes per #14 Phases 4-5.
pub const APPLE_SDK_SHAPE_ENV_VAR: &str = "SOLDR_APPLE_SDK_SHAPE";

/// Versions soldr knows how to fetch from the toolchain catalogue.
/// Bump when the catalogue grows new SDK rows per soldr-toolchain#14
/// Phase 5.
pub const SUPPORTED_APPLE_SDK_VERSIONS: &[&str] = &["11.3", "13.3", "14.5", "15.2"];

/// Apple SDK packaging shapes a consumer can ask for. See
/// [`APPLE_SDK_SHAPE_ENV_VAR`] for the wire shape; the [`Display`]
/// impl on this enum matches the catalogue-path slug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleSdkShape {
    /// `darwin-universal2` — fat artifact carrying every arch's slice.
    /// Default. Works for both `x86_64-apple-darwin` and
    /// `aarch64-apple-darwin` targets.
    Universal2,
    /// `darwin-x86_64` — lipo-thinned to x86_64 only. Smaller per-
    /// download cost; only valid for `x86_64-apple-darwin` builds.
    ThinX86_64,
    /// `darwin-aarch64` — lipo-thinned to arm64 only. Smaller per-
    /// download cost; only valid for `aarch64-apple-darwin` builds.
    ThinAArch64,
}

impl AppleSdkShape {
    /// Catalogue-path slug matching the soldr-toolchain layout:
    /// `apple-sdk/<version>/<slug>/sdk.tar.zst`.
    pub fn catalogue_slug(&self) -> &'static str {
        match self {
            AppleSdkShape::Universal2 => "darwin-universal2",
            AppleSdkShape::ThinX86_64 => "darwin-x86_64",
            AppleSdkShape::ThinAArch64 => "darwin-aarch64",
        }
    }
}

/// Resolve the Apple SDK shape soldr should fetch for `target`.
/// Precedence: [`APPLE_SDK_SHAPE_ENV_VAR`] > auto-based-on-target >
/// `Universal2` default.
pub fn resolve_apple_sdk_shape(target_triple: Option<&str>) -> AppleSdkShape {
    if let Ok(value) = std::env::var(APPLE_SDK_SHAPE_ENV_VAR) {
        match value.trim().to_ascii_lowercase().as_str() {
            "universal2" => return AppleSdkShape::Universal2,
            "thin-x86_64" | "thin_x86_64" => return AppleSdkShape::ThinX86_64,
            "thin-aarch64" | "thin_aarch64" => return AppleSdkShape::ThinAArch64,
            "auto" | "" => {}
            other => {
                eprintln!(
                    "soldr: warning: {APPLE_SDK_SHAPE_ENV_VAR}={other:?} not recognized; \
                     falling back to auto."
                );
            }
        }
    }
    // Auto: pick the thin variant matching the target triple's arch,
    // universal2 otherwise. Conservative default = universal2 so the
    // existing rust-toolchain.toml flow doesn't break when the
    // catalogue rows for thin shapes haven't been populated yet
    // (soldr-toolchain#14 Phase 4-5).
    let Some(triple) = target_triple else {
        return AppleSdkShape::Universal2;
    };
    if triple.starts_with("x86_64-apple-darwin") {
        AppleSdkShape::ThinX86_64
    } else if triple.starts_with("aarch64-apple-darwin") {
        AppleSdkShape::ThinAArch64
    } else {
        AppleSdkShape::Universal2
    }
}

/// Resolve the Apple SDK version soldr should fetch. Precedence:
/// [`APPLE_SDK_VERSION_ENV_VAR`] > [`MANAGED_APPLE_SDK_VERSION`].
pub fn resolve_apple_sdk_version() -> String {
    if let Ok(value) = std::env::var(APPLE_SDK_VERSION_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            if SUPPORTED_APPLE_SDK_VERSIONS.contains(&trimmed) {
                return trimmed.to_string();
            }
            eprintln!(
                "soldr: warning: {APPLE_SDK_VERSION_ENV_VAR}={trimmed:?} not in supported set \
                 {SUPPORTED_APPLE_SDK_VERSIONS:?}; falling back to {MANAGED_APPLE_SDK_VERSION}."
            );
        }
    }
    MANAGED_APPLE_SDK_VERSION.to_string()
}

/// soldr-toolchain#14 Phase 6 — URL substring identifying the
/// catalogue row for a given (version, shape). The catalogue resolver
/// uses this to disambiguate when multiple SDK rows exist for the
/// same `(owner, repo, tag, asset)` tuple.
///
/// Format: `/apple-sdk/<version>/<shape-slug>/`
pub fn catalogue_url_substr(version: &str, shape: AppleSdkShape) -> String {
    format!("/apple-sdk/{version}/{}/", shape.catalogue_slug())
}

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

    // soldr-toolchain#14 Phase 6 — shape + version picker.

    crate::timed_test!(shape_slugs_match_catalogue_layout, {
        assert_eq!(
            AppleSdkShape::Universal2.catalogue_slug(),
            "darwin-universal2"
        );
        assert_eq!(AppleSdkShape::ThinX86_64.catalogue_slug(), "darwin-x86_64");
        assert_eq!(
            AppleSdkShape::ThinAArch64.catalogue_slug(),
            "darwin-aarch64"
        );
    });

    crate::timed_test!(catalogue_url_substr_format, {
        assert_eq!(
            catalogue_url_substr("14.5", AppleSdkShape::Universal2),
            "/apple-sdk/14.5/darwin-universal2/"
        );
        assert_eq!(
            catalogue_url_substr("11.3", AppleSdkShape::ThinX86_64),
            "/apple-sdk/11.3/darwin-x86_64/"
        );
        assert_eq!(
            catalogue_url_substr("15.2", AppleSdkShape::ThinAArch64),
            "/apple-sdk/15.2/darwin-aarch64/"
        );
    });

    // Env-mutation tests are consolidated into one serial block so
    // libtest's parallel-by-default scheduler doesn't race two of
    // them on the same env var (the apple_sdk picker reads both
    // APPLE_SDK_SHAPE_ENV_VAR and APPLE_SDK_VERSION_ENV_VAR).
    crate::timed_test!(picker_env_var_behaviour_serial, {
        let shape_prev = std::env::var_os(APPLE_SDK_SHAPE_ENV_VAR);
        let version_prev = std::env::var_os(APPLE_SDK_VERSION_ENV_VAR);

        // --- auto-by-target when env unset ---
        std::env::remove_var(APPLE_SDK_SHAPE_ENV_VAR);
        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-apple-darwin")),
            AppleSdkShape::ThinX86_64
        );
        assert_eq!(
            resolve_apple_sdk_shape(Some("aarch64-apple-darwin")),
            AppleSdkShape::ThinAArch64
        );
        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-unknown-linux-gnu")),
            AppleSdkShape::Universal2,
            "non-darwin target falls back to universal2"
        );
        assert_eq!(
            resolve_apple_sdk_shape(None),
            AppleSdkShape::Universal2,
            "missing target falls back to universal2"
        );

        // --- env-var override (case-insensitive + underscore alias) ---
        for (env_value, expected) in [
            ("universal2", AppleSdkShape::Universal2),
            ("thin-x86_64", AppleSdkShape::ThinX86_64),
            ("thin-aarch64", AppleSdkShape::ThinAArch64),
            ("Universal2", AppleSdkShape::Universal2),
            ("thin_x86_64", AppleSdkShape::ThinX86_64),
        ] {
            std::env::set_var(APPLE_SDK_SHAPE_ENV_VAR, env_value);
            assert_eq!(
                resolve_apple_sdk_shape(Some("aarch64-apple-darwin")),
                expected,
                "{env_value} should override target-arch detection"
            );
        }
        // Empty / "auto" → fall back to target-derived
        std::env::set_var(APPLE_SDK_SHAPE_ENV_VAR, "auto");
        assert_eq!(
            resolve_apple_sdk_shape(Some("x86_64-apple-darwin")),
            AppleSdkShape::ThinX86_64
        );

        // --- version picker ---
        std::env::set_var(APPLE_SDK_VERSION_ENV_VAR, "14.5");
        assert_eq!(resolve_apple_sdk_version(), "14.5");
        std::env::set_var(APPLE_SDK_VERSION_ENV_VAR, "99.99");
        assert_eq!(
            resolve_apple_sdk_version(),
            MANAGED_APPLE_SDK_VERSION,
            "unsupported version falls back to default"
        );
        std::env::remove_var(APPLE_SDK_VERSION_ENV_VAR);
        assert_eq!(resolve_apple_sdk_version(), MANAGED_APPLE_SDK_VERSION);

        // Restore both env vars to whatever the test runner had.
        match shape_prev {
            Some(v) => std::env::set_var(APPLE_SDK_SHAPE_ENV_VAR, v),
            None => std::env::remove_var(APPLE_SDK_SHAPE_ENV_VAR),
        }
        match version_prev {
            Some(v) => std::env::set_var(APPLE_SDK_VERSION_ENV_VAR, v),
            None => std::env::remove_var(APPLE_SDK_VERSION_ENV_VAR),
        }
    });

    crate::timed_test!(supported_versions_include_default, {
        // The compat default MUST always be in the supported set;
        // otherwise a fresh install with no env var falls through to
        // an unsupported version error.
        assert!(SUPPORTED_APPLE_SDK_VERSIONS.contains(&MANAGED_APPLE_SDK_VERSION));
    });
}
