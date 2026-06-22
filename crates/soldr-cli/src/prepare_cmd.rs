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
    "https://media.githubusercontent.com/media/zackees/soldr/manifest/deps/xwin-cache/2026-06-22b/xwin-cache.tar.zst";

/// Pinned sha256 of the xwin cache tar.zst. Mismatch is a hard error.
/// `2026-06-22b`: re-vendored to preserve cargo-xwin's case-sensitive
/// symlinks (`windows.h` → `Windows.h`, etc.) that the prior tarball
/// dropped when packaged from a Windows-host-mounted docker volume.
/// Without those symlinks Linux CI's clang-cl can't find lowercased
/// includes — every Windows lane failed with "windows.h: file not
/// found" in run 27931497526. See #901.
pub const XWIN_CACHE_SHA256: &str =
    "957a51e5738d1352c18bd14caa664a88099e2f4e78afaf94f911f0cb925745fa";

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

/// Resolve the xwin cache root cargo-xwin uses. cargo-xwin's default is
/// `dirs::cache_dir().join("cargo-xwin")`. On Linux that resolves to
/// `$XDG_CACHE_HOME/cargo-xwin` (when set) or `$HOME/.cache/cargo-xwin`.
/// We mirror that here so the extraction lands at the exact path
/// cargo-xwin will look in.
fn xwin_cache_root() -> Result<PathBuf, SoldrError> {
    let home = crate::core::home_dir()?;
    #[cfg(target_os = "linux")]
    let cache_base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(".cache"),
    };
    #[cfg(target_os = "macos")]
    let cache_base = home.join("Library").join("Caches");
    #[cfg(target_os = "windows")]
    let cache_base = match std::env::var_os("LOCALAPPDATA") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join("AppData").join("Local"),
    };
    Ok(cache_base.join("cargo-xwin"))
}

/// Extract the vendored xwin cache into the location cargo-xwin reads
/// by default. After extraction the layout is
/// `<cache>/xwin/{crt,sdk,DONE,...}`. The DONE file in the tarball was
/// written by `cargo xwin cache xwin` itself when we pre-populated it
/// — its first line lists the arches (`x86_64 aarch64`) and subsequent
/// lines list every file downloaded. cargo-xwin reads that exact
/// format to decide whether to skip the live download.
///
/// IMPORTANT: do NOT overwrite `<cache>/xwin/DONE` after extraction.
/// The tarball already contains the correct cargo-xwin marker; writing
/// anything else there (e.g. a sha256 hex string as we used to) makes
/// cargo-xwin treat the first line as an unknown "arch", fail the
/// arch-match check, and re-download MSVC CRT live (15min wasted).
/// We use `.soldr-extracted` as a sibling marker for our own
/// idempotence instead.
async fn ensure_xwin_cache() -> Result<PathBuf, SoldrError> {
    let cache_root = xwin_cache_root()?;
    let xwin_dir = cache_root.join("xwin");
    // Sibling marker for our own already-extracted check — must NOT
    // be `DONE` (cargo-xwin owns that filename) and must NOT be inside
    // `xwin/` if any file there is being checksummed by cargo-xwin.
    let soldr_marker = cache_root.join(".soldr-xwin-extracted");

    if soldr_marker.is_file() && xwin_dir.join("crt").join("include").is_dir() {
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
    // Drop our own marker SIBLING to xwin/, not inside it — see the
    // doc comment above for why clobbering xwin/DONE was a 15-min CI
    // regression.
    std::fs::write(&soldr_marker, XWIN_CACHE_SHA256)?;
    eprintln!(
        "soldr prepare: extracted xwin cache to {} (size {} bytes)",
        xwin_dir.display(),
        bytes.len()
    );
    Ok(xwin_dir)
}

/// Top-level entry point for `soldr prepare --target <triple>`.
pub async fn run(
    target: String,
    github_env: Option<PathBuf>,
    save: Option<PathBuf>,
    restore: Option<PathBuf>,
) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let github_env_path = github_env.as_deref();

    eprintln!("soldr prepare: target={target}");

    // `--restore`: extract a previously-saved archive of soldr-managed
    // prepare state (zig, LLVM, Apple SDK, xwin cache) BEFORE running
    // the normal prepare flow. Anything still missing afterwards gets
    // downloaded by the normal dispatch below. Restore failures are
    // logged but non-fatal — partial cache hits still help.
    if let Some(archive) = restore.as_deref() {
        match restore_prepare_state(archive, &paths) {
            Ok(()) => eprintln!("soldr prepare: restored state from {}", archive.display()),
            Err(e) => eprintln!(
                "soldr prepare: warning: restore from {} failed: {e}; will re-download as needed",
                archive.display()
            ),
        }
    }

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

    // `--save`: capture the prepared state into a tar.zst that callers
    // can plug into `actions/cache@v4`'s save step. Subsequent CI runs
    // pass the same path to `--restore` and skip the live downloads.
    if let Some(archive) = save.as_deref() {
        save_prepare_state(archive, &paths)?;
        eprintln!("soldr prepare: saved state to {}", archive.display());
    }

    eprintln!("soldr prepare: done");
    Ok(())
}

/// Worker count for the zstd encoder. `std::thread::available_parallelism`
/// is the most portable read of host parallelism; saturate to a sane
/// upper bound so we don't spawn a hundred zstd workers on big runners.
fn num_cpus_for_zstd() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8) as u32)
        .unwrap_or(4)
}

/// Glob-style listing of soldr-managed dirs that `prepare` populates.
/// Captured by `--save`, restored by `--restore`. Paths are relative
/// to the user's HOME dir so the archive is portable across runners
/// that share the same host triple.
///
/// We pack ENTIRE versioned subdirs (e.g. `~/.soldr/bin/zig-0.13.0/`)
/// rather than the parent `~/.soldr/bin/` so the archive doesn't
/// accidentally pull in zccache binaries or anything unrelated.
fn prepare_state_roots(paths: &SoldrPaths) -> Result<Vec<PathBuf>, SoldrError> {
    let mut roots = Vec::new();
    // ~/.soldr/bin/{zig-<ver>,llvm-<ver>,apple-sdk/<ver>}
    if let Ok(entries) = std::fs::read_dir(&paths.bin) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("zig-") || n.starts_with("llvm-") || n == "apple-sdk" {
                roots.push(entry.path());
            }
        }
    }
    // xwin cache (cargo-xwin's default location, mirrors `xwin_cache_root`).
    let xwin_root = xwin_cache_root()?;
    if xwin_root.is_dir() {
        roots.push(xwin_root);
    }
    Ok(roots)
}

/// Pack the prepare-managed dirs into a tar.zst at `archive`. Paths
/// inside the tar are RELATIVE to HOME so restore can extract them
/// onto any runner that uses the same home layout.
fn save_prepare_state(archive: &Path, paths: &SoldrPaths) -> Result<(), SoldrError> {
    let home = crate::core::home_dir()?;
    let roots = prepare_state_roots(paths)?;
    if roots.is_empty() {
        eprintln!("soldr prepare: nothing to save (no zig/llvm/apple-sdk/xwin dirs found)");
        return Ok(());
    }

    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(archive)?;
    // Level 12 + multi-thread: balances compression ratio and wall-clock.
    // -19 (the manifest-branch durable archive setting) is ~10x slower
    // single-threaded; -3 is fastest but bloats the archive. Multi-thread
    // (`-T0` equivalent) lets level 12 finish in a fraction of -19's
    // wall time while keeping the archive small enough to fit GHA's
    // 10 GiB per-repo cache budget comfortably.
    let mut encoder = zstd::stream::write::Encoder::new(file, 12)
        .map_err(|e| SoldrError::Archive(format!("zstd encoder init: {e}")))?;
    let _ = encoder.multithread(num_cpus_for_zstd());
    let encoder = encoder.auto_finish();
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for root in &roots {
        let rel = match root.strip_prefix(&home) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "soldr prepare: warning: {} is outside HOME ({}); skipping",
                    root.display(),
                    home.display()
                );
                continue;
            }
        };
        eprintln!("soldr prepare: saving {}", rel.display());
        // tar append can fail on Windows NTFS reparse points (junctions)
        // that cargo-xwin creates locally; on Linux CI runners these
        // are POSIX symlinks and tar handles them natively. Log + skip
        // failures rather than aborting the whole save — partial
        // archives still help the next restore.
        if let Err(e) = builder.append_dir_all(rel, root) {
            eprintln!(
                "soldr prepare: warning: tar append {} failed: {e}; skipping",
                rel.display()
            );
        }
    }
    builder
        .finish()
        .map_err(|e| SoldrError::Archive(format!("tar finish: {e}")))?;
    Ok(())
}

/// Extract a previously-saved tar.zst back onto disk. Entries are
/// resolved relative to HOME so the same archive replays across any
/// runner that shares the home layout. Existing files are overwritten
/// — the caller (`--restore`) treats partial / outdated archives as
/// best-effort: anything still missing after restore is re-downloaded
/// by the normal dispatch.
fn restore_prepare_state(archive: &Path, _paths: &SoldrPaths) -> Result<(), SoldrError> {
    let home = crate::core::home_dir()?;
    let file = std::fs::File::open(archive)
        .map_err(|e| SoldrError::Other(format!("open {}: {e}", archive.display())))?;
    let zst = zstd::stream::read::Decoder::new(file)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder: {e}")))?;
    let mut tarball = tar::Archive::new(zst);
    std::fs::create_dir_all(&home)?;
    tarball
        .unpack(&home)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
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
