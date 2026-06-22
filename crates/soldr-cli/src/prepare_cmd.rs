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

    // Classify the triple up front so the dispatch below + the
    // post-restore audit + any future per-target cache namespacing all
    // share the same source of truth. Unknown triples ERROR here
    // instead of silently falling through to a no-op.
    let attrs = classify_target(&target)?;

    eprintln!("soldr prepare: target={target}");

    // `--restore`: extract a previously-saved archive of soldr-managed
    // prepare state (zig, LLVM, Apple SDK, xwin cache) BEFORE running
    // the normal prepare flow. Anything still missing afterwards gets
    // downloaded by the normal dispatch below. Restore failures are
    // logged but non-fatal — partial cache hits still help.
    //
    // After restore, walk the expected paths for `target` and emit a
    // present/missing summary so consumers can see whether the cache
    // covered everything or the dispatch will need to re-download
    // pieces. Missing paths are NOT an error — they just trigger
    // normal downloads via the dispatch below (#900 acceptance).
    if let Some(archive) = restore.as_deref() {
        match restore_prepare_state(archive, &paths) {
            Ok(()) => eprintln!("soldr prepare: restored state from {}", archive.display()),
            Err(e) => eprintln!(
                "soldr prepare: warning: restore from {} failed: {e}; will re-download as needed",
                archive.display()
            ),
        }
        // Emit the audit even if restore raised — partial restores are
        // useful and the dispatch fills any remaining gaps. The
        // present/missing summary lets consumers see exactly which
        // pieces survived.
        let report = expected_state_paths(&attrs, &paths)?;
        emit_restore_report(&report);
    }

    // Always add the rustup target (idempotent).
    if let Err(e) = rustup_add_target(&target) {
        eprintln!("soldr prepare: warning: rustup target add failed: {e}");
    }

    match attrs.os {
        TargetOs::Windows => {
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
        }
        TargetOs::Darwin => {
            // Darwin cross-compile path: needs zig + Apple SDK; export SDKROOT.
            eprintln!("soldr prepare: dispatch=zigbuild+apple-sdk");
            let zig_dir = ensure_zig(&paths).await?;
            eprintln!("soldr prepare: zig at {}", zig_dir.display());
            let sdk = ensure_apple_sdk(&paths).await?;
            eprintln!("soldr prepare: Apple SDK at {}", sdk.display());
            let sdk_str = sdk.to_string_lossy();
            println!("SDKROOT={sdk_str}");
            append_env(github_env_path, "SDKROOT", &sdk_str)?;
        }
        TargetOs::Linux => {
            // Linux cross-compile via zigbuild (musl always, gnu when
            // host != target arch).
            eprintln!("soldr prepare: dispatch=zigbuild");
            let zig_dir = ensure_zig(&paths).await?;
            eprintln!("soldr prepare: zig at {}", zig_dir.display());
        }
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

/// One row in the per-target post-restore validation report.
/// `present` is true when the expected path exists on disk; the
/// `path` field is the location consumers can grep for in logs.
#[derive(Debug, Clone)]
struct RestoreEntry {
    label: String,
    path: PathBuf,
    present: bool,
}

/// CPU architecture tag in a Rust target triple. Restricted to the
/// arches soldr's cross-compile bootstrap supports; anything else
/// causes `classify_target` to ERROR rather than falling through
/// to a no-op dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetArch {
    X86_64,
    Aarch64,
}

/// OS family in a Rust target triple. Limited to the cross-compile
/// destinations soldr supports today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Windows,
    Darwin,
    Linux,
}

/// ABI suffix in a Rust target triple. `None` for darwin (no ABI
/// suffix in apple triples). `Msvc`/`Gnu`/`Musl` for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAbi {
    Msvc,
    Gnu,
    Musl,
}

/// Result of classifying a Rust target triple into the soldr
/// bootstrap dispatch attributes. Replaces the ad-hoc
/// `target.ends_with("...")` checks with a single source-of-truth
/// classifier — every consumer (the dispatch in `run()`, the
/// restore-audit's `expected_state_paths`, future per-target cache
/// namespacing) reads off the same struct.
///
/// `classify_target` ERRORs on unknown triples rather than silently
/// returning an empty attribute set, so a typo in CI YAML surfaces
/// loudly instead of pretending the prepare succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAttrs {
    /// The canonical triple as supplied.
    pub triple: String,
    pub arch: TargetArch,
    pub os: TargetOs,
    pub abi: Option<TargetAbi>,
    /// Needs `cargo-zigbuild` (and zig on PATH). True for cross-
    /// compile to darwin or to a non-host linux flavor.
    pub needs_zig: bool,
    /// Needs the vendored MSVC CRT + Windows SDK cache extracted to
    /// `~/.cache/cargo-xwin/`. True for `*-pc-windows-msvc`.
    pub needs_xwin_cache: bool,
    /// Needs the soldr-managed LLVM toolchain (clang / lld-link /
    /// llvm-lib). True for `*-pc-windows-msvc` (cargo-xwin uses it).
    pub needs_llvm_toolchain: bool,
    /// Needs the vendored Apple SDK (IOKit/CoreFoundation/...).
    /// True for `*-apple-darwin`.
    pub needs_apple_sdk: bool,
}

/// Classify a Rust target triple into a `TargetAttrs`. Pattern is
/// the standard `<arch>-<vendor>-<os>[-<abi>]` triple structure
/// LLVM uses; `vendor` is ignored (always `pc`, `apple`, or
/// `unknown` for the targets we care about). Returns an error for
/// any triple soldr's bootstrap doesn't know how to prepare — a
/// typo in CI surfaces as a hard failure instead of a silent
/// no-op.
pub fn classify_target(triple: &str) -> Result<TargetAttrs, SoldrError> {
    // Tokenize on `-`. Accept `<arch>-<vendor>-<os>` (3 parts, darwin)
    // and `<arch>-<vendor>-<os>-<abi>` (4 parts, windows/linux).
    let parts: Vec<&str> = triple.split('-').collect();
    if parts.len() < 3 || parts.len() > 4 {
        return Err(SoldrError::Other(format!(
            "soldr prepare: unrecognized target triple shape: `{triple}` \
             (expected `<arch>-<vendor>-<os>[-<abi>]`)"
        )));
    }
    let arch = match parts[0] {
        "x86_64" => TargetArch::X86_64,
        "aarch64" => TargetArch::Aarch64,
        other => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported arch `{other}` in triple `{triple}` \
                 (supported: x86_64, aarch64)"
            )));
        }
    };
    // parts[1] is vendor (`pc`, `apple`, `unknown`) — accepted as-is.
    let os_str = parts[2];
    let abi_str = parts.get(3).copied();
    let (os, abi) = match (os_str, abi_str) {
        ("windows", Some("msvc")) => (TargetOs::Windows, Some(TargetAbi::Msvc)),
        ("darwin", None) => (TargetOs::Darwin, None),
        ("linux", Some("gnu")) => (TargetOs::Linux, Some(TargetAbi::Gnu)),
        ("linux", Some("musl")) => (TargetOs::Linux, Some(TargetAbi::Musl)),
        _ => {
            return Err(SoldrError::Other(format!(
                "soldr prepare: unsupported os/abi combination `{os_str}{}` in triple `{triple}` \
                 (supported: windows-msvc, darwin, linux-gnu, linux-musl)",
                abi_str.map(|a| format!("-{a}")).unwrap_or_default()
            )));
        }
    };
    Ok(TargetAttrs {
        triple: triple.to_string(),
        arch,
        os,
        abi,
        needs_zig: matches!(os, TargetOs::Darwin | TargetOs::Linux),
        needs_xwin_cache: matches!(os, TargetOs::Windows),
        needs_llvm_toolchain: matches!(os, TargetOs::Windows),
        needs_apple_sdk: matches!(os, TargetOs::Darwin),
    })
}

/// List the on-disk paths that `prepare --target <triple>` is
/// expected to populate. Used after `--restore` to surface a
/// present/missing summary; the dispatch below downloads anything
/// missing so the report is purely informational.
///
/// Paths are version-pinned where possible (e.g. zig 0.13.0, LLVM
/// 21.1.5) so a stale archive that's missing the current pin is
/// reported as "missing" even if an older version exists on disk.
fn expected_state_paths(
    attrs: &TargetAttrs,
    paths: &SoldrPaths,
) -> Result<Vec<RestoreEntry>, SoldrError> {
    let mut entries = Vec::new();
    if attrs.needs_zig {
        let zig_dir = paths
            .bin
            .join(format!("zig-{}", crate::fetch::MANAGED_ZIG_VERSION));
        let present = zig_dir.join(".complete").is_file() || zig_dir.is_dir();
        entries.push(RestoreEntry {
            label: format!("zig {}", crate::fetch::MANAGED_ZIG_VERSION),
            path: zig_dir,
            present,
        });
    }
    if attrs.needs_llvm_toolchain {
        let llvm_dir = paths
            .bin
            .join(format!("llvm-{}", crate::fetch::MANAGED_LLVM_VERSION));
        entries.push(RestoreEntry {
            label: format!("LLVM {}", crate::fetch::MANAGED_LLVM_VERSION),
            present: llvm_dir.is_dir(),
            path: llvm_dir,
        });
    }
    if attrs.needs_xwin_cache {
        let xwin = xwin_cache_root()?.join("xwin");
        entries.push(RestoreEntry {
            label: "xwin MSVC CRT + Windows SDK".to_string(),
            present: xwin.join("crt").join("include").is_dir()
                && xwin.join("sdk").join("include").is_dir(),
            path: xwin,
        });
    }
    if attrs.needs_apple_sdk {
        let sdk = paths
            .bin
            .join("apple-sdk")
            .join(crate::fetch::MANAGED_APPLE_SDK_VERSION);
        entries.push(RestoreEntry {
            label: format!("Apple SDK {}", crate::fetch::MANAGED_APPLE_SDK_VERSION),
            present: sdk.join(".complete").is_file() || sdk.is_dir(),
            path: sdk,
        });
    }
    Ok(entries)
}

fn emit_restore_report(entries: &[RestoreEntry]) {
    if entries.is_empty() {
        eprintln!("soldr prepare: restore audit: target has no expected paths to check");
        return;
    }
    let present = entries.iter().filter(|e| e.present).count();
    let total = entries.len();
    eprintln!("soldr prepare: restore audit: {present}/{total} expected entries present");
    for entry in entries {
        let mark = if entry.present { "✓" } else { "✗" };
        eprintln!(
            "  {mark} {label}  ({path})",
            mark = mark,
            label = entry.label,
            path = entry.path.display()
        );
    }
    if present < total {
        eprintln!(
            "soldr prepare: restore audit: {} missing entr{} will be downloaded by dispatch",
            total - present,
            if total - present == 1 { "y" } else { "ies" }
        );
    }
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

    crate::timed_test!(classify_target_windows_msvc, {
        let attrs = classify_target("x86_64-pc-windows-msvc").expect("classify");
        assert_eq!(attrs.arch, TargetArch::X86_64);
        assert_eq!(attrs.os, TargetOs::Windows);
        assert_eq!(attrs.abi, Some(TargetAbi::Msvc));
        assert!(attrs.needs_xwin_cache);
        assert!(attrs.needs_llvm_toolchain);
        assert!(!attrs.needs_zig);
        assert!(!attrs.needs_apple_sdk);

        let arm = classify_target("aarch64-pc-windows-msvc").expect("classify arm");
        assert_eq!(arm.arch, TargetArch::Aarch64);
        assert_eq!(arm.os, TargetOs::Windows);
    });

    crate::timed_test!(classify_target_apple_darwin, {
        let attrs = classify_target("aarch64-apple-darwin").expect("classify");
        assert_eq!(attrs.arch, TargetArch::Aarch64);
        assert_eq!(attrs.os, TargetOs::Darwin);
        assert_eq!(attrs.abi, None);
        assert!(attrs.needs_zig);
        assert!(attrs.needs_apple_sdk);
        assert!(!attrs.needs_xwin_cache);
        assert!(!attrs.needs_llvm_toolchain);

        let intel = classify_target("x86_64-apple-darwin").expect("classify intel");
        assert_eq!(intel.arch, TargetArch::X86_64);
    });

    crate::timed_test!(classify_target_linux_gnu_and_musl, {
        let gnu = classify_target("x86_64-unknown-linux-gnu").expect("classify gnu");
        assert_eq!(gnu.os, TargetOs::Linux);
        assert_eq!(gnu.abi, Some(TargetAbi::Gnu));
        assert!(gnu.needs_zig);
        assert!(!gnu.needs_xwin_cache);
        assert!(!gnu.needs_apple_sdk);

        let musl = classify_target("aarch64-unknown-linux-musl").expect("classify musl");
        assert_eq!(musl.os, TargetOs::Linux);
        assert_eq!(musl.abi, Some(TargetAbi::Musl));
    });

    crate::timed_test!(classify_target_rejects_unknown_arch, {
        let err = classify_target("riscv64-unknown-linux-gnu").expect_err("riscv unsupported");
        assert!(err.to_string().contains("unsupported arch"));
    });

    crate::timed_test!(classify_target_rejects_unknown_os, {
        let err = classify_target("x86_64-unknown-freebsd").expect_err("freebsd unsupported");
        assert!(err.to_string().contains("unsupported os/abi"));
    });

    crate::timed_test!(classify_target_rejects_malformed_triple, {
        let err = classify_target("x86_64").expect_err("too few parts");
        assert!(err.to_string().contains("unrecognized target triple shape"));
        let err = classify_target("a-b-c-d-e").expect_err("too many parts");
        assert!(err.to_string().contains("unrecognized target triple shape"));
    });

    crate::timed_test!(classify_target_rejects_windows_gnu_abi, {
        // soldr supports MSVC only on Windows; gnu (mingw) is not in
        // scope so the classifier rejects it.
        let err = classify_target("x86_64-pc-windows-gnu").expect_err("gnu abi on windows");
        assert!(err.to_string().contains("unsupported os/abi"));
    });
}
