//! `soldr prepare --target <triple>` — uniform cross-compile toolchain bootstrap.
//!
//! Single CLI surface for every cross-compile target: same invocation
//! shape, only `--target` varies. Internally dispatches based on the
//! target triple:
//!
//! - `*-pc-windows-msvc` → ensure cargo-xwin + LLVM toolchain + extract
//!   the vendored xwin MSVC CRT cache from the manifest branch to
//!   `~/.cache/cargo-xwin/` so `cargo xwin build` skips the 15-min live
//!   Microsoft download.
//! - `*-apple-darwin` → ensure cargo-zigbuild + zig + Apple SDK; print
//!   `SDKROOT=<path>` so the caller can plumb it into `$GITHUB_ENV`.
//! - `*-unknown-linux-{gnu,musl}` (when triple ≠ host) → ensure
//!   cargo-zigbuild + zig.
//! - All targets: `rustup target add <triple>`.
//!
//! Designed to collapse the per-step ad-hoc downloads in
//! `cross-compile-all-targets.yml` into a single "Preparing Cross
//! Compile Toolchain" step.
//!
//! Output goes to stdout (human-readable). When `--github-env` is set,
//! also appends `KEY=VALUE` lines (e.g. `SDKROOT=/opt/...`) to that
//! file so a GitHub-Actions runner can pick them up in $GITHUB_ENV.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::github::http_client;
use crate::fetch::{ensure_apple_sdk, ensure_llvm_toolchain, ensure_zig};

/// URL of the vendored cargo-xwin MSVC CRT + Windows SDK cache on
/// soldr's `manifest` branch. PR #890 packaged 1.08 GiB of MSVC CRT +
/// Windows SDK headers/libs as a single 81 MiB zstd-19 tarball; this
/// extracts into `~/.cache/cargo-xwin/xwin/{crt,sdk}` so subsequent
/// `cargo xwin build` invocations skip the live Microsoft download.
pub const XWIN_CACHE_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr/manifest/deps/xwin-cache/2026-06-22/xwin-cache.tar.zst";

/// Pinned sha256 of the xwin cache tar.zst. Mismatch is a hard error.
pub const XWIN_CACHE_SHA256: &str =
    "33c04d8026d99dab4d66f39ddbd93d75f64c68063d4ba58e5450626524bf348d";

/// Append `KEY=VALUE` to the file at `path` (creating it if needed).
/// No-op when `path` is `None`. Used so callers running under GitHub
/// Actions can pipe env vars (SDKROOT, etc.) into `$GITHUB_ENV`.
fn append_env(path: Option<&Path>, key: &str, value: &str) -> Result<(), SoldrError> {
    if let Some(p) = path {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| SoldrError::Other(format!("open {}: {e}", p.display())))?;
        writeln!(f, "{key}={value}")
            .map_err(|e| SoldrError::Other(format!("write {}: {e}", p.display())))?;
    }
    Ok(())
}

/// Extract the vendored xwin cache into `~/.cache/cargo-xwin/`. After
/// extraction, the layout is `~/.cache/cargo-xwin/xwin/{crt,sdk}` —
/// exactly what cargo-xwin checks on `build` to decide whether to
/// invoke its live downloader.
async fn ensure_xwin_cache() -> Result<PathBuf, SoldrError> {
    let home = crate::core::home_dir()?;
    let cache_root = home.join(".cache").join("cargo-xwin");
    let xwin_dir = cache_root.join("xwin");
    let marker = xwin_dir.join("DONE");

    if marker.is_file() && xwin_dir.join("crt").join("include").is_dir() {
        eprintln!(
            "soldr prepare: xwin cache already present at {}",
            xwin_dir.display()
        );
        return Ok(xwin_dir);
    }

    eprintln!("soldr prepare: fetching xwin MSVC CRT + Windows SDK cache from {XWIN_CACHE_URL}...");
    let client = http_client()?;
    let resp = client
        .get(XWIN_CACHE_URL)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "xwin cache download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    let digest = crate::fetch::trust::sha256_of(&bytes);
    if digest != XWIN_CACHE_SHA256 {
        return Err(SoldrError::Other(format!(
            "xwin cache sha256 mismatch: expected {XWIN_CACHE_SHA256}, got {digest}"
        )));
    }
    eprintln!("soldr prepare: trust: verified xwin cache sha256={digest}");

    std::fs::create_dir_all(&cache_root)?;
    let reader = std::io::Cursor::new(&bytes[..]);
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive
        .unpack(&cache_root)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    std::fs::write(&marker, XWIN_CACHE_SHA256)?;
    eprintln!(
        "soldr prepare: extracted xwin cache to {} (size {} bytes)",
        xwin_dir.display(),
        bytes.len()
    );
    Ok(xwin_dir)
}

/// Top-level entry point for `soldr prepare --target <triple>`.
pub async fn run(target: String, github_env: Option<PathBuf>) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let github_env_path = github_env.as_deref();

    eprintln!("soldr prepare: target={target}");

    // Always add the rustup target (idempotent).
    if let Err(e) = rustup_add_target(&target) {
        eprintln!("soldr prepare: warning: rustup target add failed: {e}");
    }

    if target.ends_with("-pc-windows-msvc") {
        // Windows MSVC cross-compile path: needs cargo-xwin, LLVM
        // toolchain (for clang/lld-link), and the MSVC CRT cache.
        eprintln!("soldr prepare: dispatch=xwin");
        let llvm_dir = match ensure_llvm_toolchain(&paths).await {
            Ok(p) => p,
            Err(SoldrError::UnsupportedPlatform(m)) => {
                eprintln!("soldr prepare: LLVM auto-bootstrap not supported on host: {m}");
                PathBuf::new()
            }
            Err(e) => return Err(e),
        };
        if !llvm_dir.as_os_str().is_empty() {
            eprintln!("soldr prepare: LLVM toolchain at {}", llvm_dir.display());
        }
        ensure_xwin_cache().await?;
    } else if target.ends_with("-apple-darwin") {
        // Darwin cross-compile path: needs zig + Apple SDK; export SDKROOT.
        eprintln!("soldr prepare: dispatch=zigbuild+apple-sdk");
        let zig_dir = ensure_zig(&paths).await?;
        eprintln!("soldr prepare: zig at {}", zig_dir.display());
        let sdk = ensure_apple_sdk(&paths).await?;
        eprintln!("soldr prepare: Apple SDK at {}", sdk.display());
        let sdk_str = sdk.to_string_lossy();
        println!("SDKROOT={sdk_str}");
        append_env(github_env_path, "SDKROOT", &sdk_str)?;
    } else if target.contains("-unknown-linux-") {
        // Linux cross-compile via zigbuild (musl always, gnu when
        // host != target arch).
        eprintln!("soldr prepare: dispatch=zigbuild");
        let zig_dir = ensure_zig(&paths).await?;
        eprintln!("soldr prepare: zig at {}", zig_dir.display());
    } else {
        eprintln!(
            "soldr prepare: target {target} has no cross-compile bootstrap recipe; \
             rustup target add only"
        );
    }

    eprintln!("soldr prepare: done");
    Ok(())
}

/// Run `rustup target add <triple>` for the active toolchain.
/// Idempotent — already-installed targets are a no-op.
fn rustup_add_target(triple: &str) -> Result<(), SoldrError> {
    let status = std::process::Command::new("rustup")
        .args(["target", "add", triple])
        .status()
        .map_err(|e| SoldrError::Other(format!("rustup target add: {e}")))?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "rustup target add {triple} exited with {}",
            status
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(constants_are_well_formed, {
        assert!(XWIN_CACHE_URL.starts_with("https://"));
        assert!(XWIN_CACHE_URL.ends_with(".tar.zst"));
        assert_eq!(XWIN_CACHE_SHA256.len(), 64);
        assert!(XWIN_CACHE_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
    });

    crate::timed_test!(append_env_creates_file_and_appends, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let p = tmp.path().join("env");
        append_env(Some(&p), "FOO", "bar").expect("append");
        append_env(Some(&p), "BAZ", "/some/path").expect("append");
        let body = std::fs::read_to_string(&p).expect("read");
        assert!(body.contains("FOO=bar"));
        assert!(body.contains("BAZ=/some/path"));
    });

    crate::timed_test!(append_env_no_op_when_none, {
        append_env(None, "FOO", "bar").expect("no-op");
    });
}
