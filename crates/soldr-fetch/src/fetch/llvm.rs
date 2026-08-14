//! Auto-bootstrap an LLVM toolchain (clang, clang-cl, lld-link, llvm-lib,
//! llvm-ar, llvm-objcopy, llvm-strip, …) for cross-compile lanes that
//! cannot rely on a stock `apt install clang lld llvm` being on the
//! runner.
//!
//! The primary consumers are the blessed `soldr build --target
//! *-pc-windows-msvc` path and the explicit `cargo xwin build`
//! fallback. Both need `clang-cl` (the MSVC-compat clang driver),
//! `lld-link` (LLD's PE/COFF linker), and `llvm-lib` (for static
//! archive creation). On a stock GitHub Linux runner those binaries
//! either don't exist or are mismatched versions across the toolchain
//! — which previously forced the workflow to `apt install llvm clang
//! lld` ahead of every job.
//!
//! Mirrors PR #841 (`ensure_zig`) and PR #862 (`ensure_apple_sdk`):
//! soldr is the bootstrapper (CLAUDE.md "Pre-built first"), so every
//! consumer of `soldr build --target *-pc-windows-msvc` or explicit
//! `soldr cargo xwin build --target *-pc-windows-msvc` Just Works on a
//! stock device. Closes #855 (sub of meta #853).
//!
//! Resolution order:
//!   1. `SOLDR_LLVM_DIR` env var pointing at an existing directory →
//!      use it verbatim (escape hatch for advanced users + reproducibility
//!      pin in CI lanes that build LLVM from source).
//!   2. Managed fetch from
//!      `https://media.githubusercontent.com/media/zackees/clang-tool-chain-bins/main/assets/clang/<plat>/<arch>/llvm-<ver>-<plat>-<arch>.tar.zst`,
//!      sha256-pinned, cached at
//!      `~/.soldr/bin/llvm-<MANAGED_LLVM_VERSION>/`.
//!
//! Supported managed hosts (see `host_llvm_asset`):
//!   - `x86_64-unknown-linux-gnu` (the GHA-hosted xwin lane)
//!   - `aarch64-unknown-linux-gnu`
//!   - `x86_64-pc-windows-msvc`
//!
//! Linux musl hosts are intentionally not mapped to the glibc Linux
//! archive. That binary can start under Alpine's `gcompat`, but `lld-link`
//! currently requires glibc symbols such as `mallinfo2`; use a glibc host or
//! provide `SOLDR_LLVM_DIR` until a musl LLVM archive exists.
//!
//! macOS hosts (Apple Silicon + Intel) are at a slightly newer LLVM
//! release upstream (21.1.6 vs 21.1.5). Rather than pin two versions,
//! the macOS case is **not** wired through managed fetch today — those
//! hosts already ship Apple's `clang` via Xcode, and the xwin → MSVC
//! cross-compile is not a primary mac lane. If a future caller needs
//! it, add a per-host version constant and a second pin row to
//! `host_llvm_asset`. The current behavior on those hosts is "log +
//! return Ok(None)-ish from `host_llvm_asset` so `ensure_llvm_toolchain`
//! surfaces an UnsupportedPlatform error" — callers that gate this
//! bootstrap behind a target-triple check (e.g. only on
//! `--target *-pc-windows-msvc`) will not reach the error path on a
//! non-cross build.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{SoldrError, SoldrPaths};

use super::stream_download::{
    asset_http_client_with_protocol, get_request, send_asset_request, stream_response_to_temp_file,
    AssetProtocol, DownloadedAsset, ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};

/// Pinned LLVM version that soldr's managed bootstrap ships for the
/// xwin lane. 21.1.5 is the latest release on
/// `zackees/clang-tool-chain-bins` that covers **both** linux-x86_64
/// and win-x86_64 (the two host targets the xwin GHA lane runs from).
/// Bumping is a per-host version table — if upstream goes ahead on
/// one host and lags on another, hold the pin until both are aligned
/// so users on either host get the same toolchain.
pub const MANAGED_LLVM_VERSION: &str = "21.1.5";

/// Env-var override: when set to an existing directory, soldr uses
/// that LLVM install as-is (no fetch, no SHA check). The directory
/// is expected to be the **bin dir** containing `clang-cl` / `lld-link`
/// / `llvm-lib` — i.e. the same shape this module returns from
/// `ensure_llvm_toolchain` after a managed fetch.
const LLVM_DIR_ENV_VAR: &str = "SOLDR_LLVM_DIR";
/// soldr#2132 called `mod.rs`, `llvm.rs` and `zig.rs` "three near-identical
/// copies" of the retry loop. They are near-identical in the retrying; they
/// differ exactly where it matters, in what happens once attempts run out:
///
/// * `mod.rs` falls back to the newest cached tool for `VersionSpec::Latest`
///   (#1879) so a rate-limited release API does not turn into a red build;
/// * `zig.rs` has target-specific archive extraction;
/// * this one simply fails, which is what `retry::with_backoff` does.
///
/// Only this one could move to the shared helper. Folding either of the others
/// would silently delete a fallback, so they keep their loops.
const LLVM_DOWNLOAD_ATTEMPTS: u32 = 4;
const LLVM_DOWNLOAD_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const _: () = assert!(LLVM_DOWNLOAD_ATTEMPTS >= 2);

/// Asset descriptor for one host triple. Hard-coded URL + sha256
/// because the upstream `manifest.json`s are not versioned per-archive
/// (a tampered manifest would let an attacker swap binaries silently).
/// Generated by reading
/// `https://raw.githubusercontent.com/zackees/clang-tool-chain-bins/main/assets/clang/<plat>/<arch>/manifest.json`
/// for `MANAGED_LLVM_VERSION`. Bump alongside the version constant.
struct LlvmAsset {
    /// Human-readable triple matching what `clang-tool-chain-bins`
    /// names: `<plat>-<arch>` (e.g. `linux-x86_64`, `win-x86_64`).
    plat_arch: &'static str,
    url: &'static str,
    sha256: &'static str,
}

const LLVM_ASSETS: &[(&str, LlvmAsset)] = &[
    (
        "x86_64-unknown-linux-gnu",
        LlvmAsset {
            plat_arch: "linux-x86_64",
            url: "https://media.githubusercontent.com/media/zackees/clang-tool-chain-bins/main/assets/clang/linux/x86_64/llvm-21.1.5-linux-x86_64.tar.zst",
            sha256: "4021cc49d70472122761709e7376835dfc857b5ec77183fa969b5f61d0f13a2f",
        },
    ),
    (
        "aarch64-unknown-linux-gnu",
        LlvmAsset {
            plat_arch: "linux-arm64",
            url: "https://media.githubusercontent.com/media/zackees/clang-tool-chain-bins/main/assets/clang/linux/arm64/llvm-21.1.5-linux-arm64.tar.zst",
            sha256: "df774b7fc1e392458325552addb67bf8c11bd452ad7bc660cf77103c617f89c5",
        },
    ),
    (
        "x86_64-pc-windows-msvc",
        LlvmAsset {
            plat_arch: "win-x86_64",
            url: "https://media.githubusercontent.com/media/zackees/clang-tool-chain-bins/main/assets/clang/win/x86_64/llvm-21.1.5-win-x86_64.tar.zst",
            sha256: "8d6dd1cbc2261f8e6fa657b48f10a6e44223441d4b5487f056838cb8c2403a77",
        },
    ),
];

/// Ensure an LLVM toolchain (clang, clang-cl, lld-link, llvm-lib, …)
/// is available for cross-compile lanes that need it. Returns the
/// directory holding the binaries so the caller can prepend it to
/// `PATH` on the child cargo and set `CC_<triple>` / `LD_<triple>` /
/// `AR_<triple>` to absolute paths inside it.
///
/// On hosts not in the managed support matrix (see `LLVM_ASSETS`),
/// returns `SoldrError::UnsupportedPlatform`. The caller should gate
/// this bootstrap behind the same target-triple check it uses to
/// decide whether the binaries are needed at all — non-cross builds
/// never reach the error path.
pub async fn ensure_llvm_toolchain(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    if let Some(dir) = llvm_dir_from_env_var() {
        return Ok(dir);
    }
    fetch_managed_llvm(paths).await
}

fn llvm_dir_from_env_var() -> Option<PathBuf> {
    let value = std::env::var_os(LLVM_DIR_ENV_VAR)?;
    let p = PathBuf::from(value);
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// Look up the asset descriptor for the running host. Returns `None`
/// on unsupported hosts (today: macOS — see module doc-comment).
fn host_llvm_asset() -> Option<&'static LlvmAsset> {
    let triple = host_triple_for_llvm()?;
    LLVM_ASSETS
        .iter()
        .find(|(t, _)| *t == triple)
        .map(|(_, asset)| asset)
}

/// Map (arch, os) to the rustc-style triple keys used in `LLVM_ASSETS`.
/// Kept separate from `core::TargetTriple` because (a) we resolve the
/// host through the facts facade, (b) we don't need rustc's
/// MSVC-vs-GNU env discrimination on Windows (only x86_64-msvc is
/// supported today).
fn host_triple_for_llvm() -> Option<&'static str> {
    use crate::platform::host::facts::{arch, libc, os, HostArch, HostLibc, HostOs};

    match (os(), arch(), libc()) {
        (HostOs::Linux, HostArch::X86_64, HostLibc::Gnu) => Some("x86_64-unknown-linux-gnu"),
        (HostOs::Linux, HostArch::Aarch64, HostLibc::Gnu) => Some("aarch64-unknown-linux-gnu"),
        (HostOs::Windows, HostArch::X86_64, _) => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

async fn fetch_managed_llvm(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    let Some(asset) = host_llvm_asset() else {
        return Err(SoldrError::UnsupportedPlatform(format!(
            "managed LLVM toolchain not available for host arch={} os={} (set {LLVM_DIR_ENV_VAR} to an existing LLVM bin dir, or extend LLVM_ASSETS)",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )));
    };

    paths.ensure_dirs()?;
    let install_dir = paths.bin.join(format!("llvm-{MANAGED_LLVM_VERSION}"));
    let bin_dir = install_dir.join("hardlinked").join("bin");
    let stamp = install_dir.join(".complete");

    if stamp.is_file() && bin_dir.is_dir() {
        return Ok(bin_dir);
    }

    eprintln!(
        "soldr: fetching LLVM v{MANAGED_LLVM_VERSION} ({}) from {}...",
        asset.plat_arch, asset.url,
    );

    let downloaded = download_llvm_asset(asset).await?;

    // Integrity check is mandatory — the upstream archive is large
    // (~95 MiB) and a tampered blob would silently install a hostile
    // toolchain. Match the apple_sdk policy: sha256 mismatch is a hard
    // refuse-to-extract error regardless of `SOLDR_TRUST_MODE`.
    let digest = downloaded.sha256();
    if digest != asset.sha256 {
        return Err(SoldrError::Other(format!(
            "LLVM sha256 mismatch: expected {expected}, got {digest} \
             (upstream archive at {url} may have been replaced — refusing to extract)",
            expected = asset.sha256,
            url = asset.url,
        )));
    }
    eprintln!(
        "soldr: trust: verified LLVM v{MANAGED_LLVM_VERSION} {} sha256={digest}",
        asset.plat_arch,
    );

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    std::fs::create_dir_all(&install_dir)?;
    extract_tar_zst_tree(std::fs::File::open(downloaded.path())?, &install_dir)?;

    if !bin_dir.is_dir() {
        return Err(SoldrError::Archive(format!(
            "LLVM extract did not produce expected directory {}",
            bin_dir.display()
        )));
    }

    std::fs::write(&stamp, MANAGED_LLVM_VERSION)?;
    eprintln!("soldr: extracted LLVM to {}", install_dir.display());
    Ok(bin_dir)
}

async fn download_llvm_asset(asset: &LlvmAsset) -> Result<DownloadedAsset, SoldrError> {
    // soldr#2132 step 1: this loop was one of three hand-rolled copies. It is
    // the only one that folds onto the shared helper without losing anything --
    // see the note on `LLVM_DOWNLOAD_ATTEMPTS` for why the other two stay.
    //
    // The old guard retried on *any* error rather than testing transience.
    // That is not a behaviour change here: every error `download_llvm_asset_once`
    // can produce is `SoldrError::Network`, which `retry::is_transient` matches,
    // so the set of retried failures is identical.
    let client = asset_http_client_with_protocol("managed LLVM", AssetProtocol::Http1Only)?;
    super::retry::with_asset_backoff_params(
        &format!("LLVM v{MANAGED_LLVM_VERSION} {}", asset.plat_arch),
        LLVM_DOWNLOAD_ATTEMPTS,
        LLVM_DOWNLOAD_INITIAL_BACKOFF,
        || download_llvm_asset_once(&client, asset),
    )
    .await
}

async fn download_llvm_asset_once(
    client: &reqwest::Client,
    asset: &LlvmAsset,
) -> Result<DownloadedAsset, SoldrError> {
    let resp = send_asset_request(
        get_request(client, asset.url).header(reqwest::header::ACCEPT_ENCODING, "identity"),
        asset.url,
        ASSET_HEADER_TIMEOUT,
    )
    .await?;
    stream_response_to_temp_file(resp, asset.url, ASSET_IDLE_TIMEOUT).await
}

fn extract_tar_zst_tree<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), SoldrError> {
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the two tests below, which are the *only* places that
    /// mutate `LLVM_DIR_ENV_VAR`.
    ///
    /// soldr#1994: they raced each other. `cargo test` runs them as threads in
    /// one process, so when `env_var_ignored_when_path_is_missing` set its
    /// deliberately-missing path between the other test's `set_var` and its
    /// read, `llvm_dir_from_env_var()` rejected the path and returned `None`.
    /// That surfaced as a `Linux x64` failure on soldr#1993, a daemon PR that
    /// touches nothing here.
    ///
    /// File-local rather than crate-wide by the rule in soldr#1896: a module
    /// guarding a variable nobody else touches keeps its own lock, because
    /// collapsing fine-grained locks over disjoint variables into one global
    /// barrier costs suite latency and buys no correctness.
    static LLVM_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_var_overrides_when_pointing_at_real_dir() {
        let _guard = LLVM_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tmpdir");
        let fake_bin = tmp.path().join("hardlinked").join("bin");
        std::fs::create_dir_all(&fake_bin).expect("mk");
        let prev = std::env::var_os(LLVM_DIR_ENV_VAR);
        std::env::set_var(LLVM_DIR_ENV_VAR, &fake_bin);
        let resolved = llvm_dir_from_env_var();
        match prev {
            Some(v) => std::env::set_var(LLVM_DIR_ENV_VAR, v),
            None => std::env::remove_var(LLVM_DIR_ENV_VAR),
        }
        assert_eq!(resolved.as_deref(), Some(fake_bin.as_path()));
    }

    #[test]
    fn env_var_ignored_when_path_is_missing() {
        let _guard = LLVM_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(LLVM_DIR_ENV_VAR);
        std::env::set_var(LLVM_DIR_ENV_VAR, "/definitely/not/a/real/path/29384720");
        let resolved = llvm_dir_from_env_var();
        match prev {
            Some(v) => std::env::set_var(LLVM_DIR_ENV_VAR, v),
            None => std::env::remove_var(LLVM_DIR_ENV_VAR),
        }
        assert!(
            resolved.is_none(),
            "missing dir should be ignored: {resolved:?}",
        );
    }

    #[test]
    fn constants_are_well_formed() {
        // Smoke: catch typos in any pinned URL / sha256 entry during a
        // version bump. Mirrors apple_sdk's `constants_are_well_formed`.
        assert!(!MANAGED_LLVM_VERSION.is_empty());
        assert!(MANAGED_LLVM_VERSION
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.'));
        assert!(
            !LLVM_ASSETS.is_empty(),
            "LLVM_ASSETS must list at least one host",
        );
        for (triple, asset) in LLVM_ASSETS {
            assert!(
                triple.contains('-'),
                "triple should look rustc-style: {triple}",
            );
            assert!(
                asset.url.starts_with("https://"),
                "{triple}: url must be https, got {}",
                asset.url,
            );
            assert!(
                asset.url.ends_with(".tar.zst"),
                "{triple}: url must end .tar.zst, got {}",
                asset.url,
            );
            assert!(
                asset.url.contains(MANAGED_LLVM_VERSION),
                "{triple}: url should embed MANAGED_LLVM_VERSION ({MANAGED_LLVM_VERSION}), got {}",
                asset.url,
            );
            assert_eq!(asset.sha256.len(), 64, "{triple}: sha256 must be 64 hex");
            assert!(
                asset.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{triple}: sha256 must be hex, got {}",
                asset.sha256,
            );
            assert!(
                !asset.plat_arch.is_empty(),
                "{triple}: plat_arch must be non-empty",
            );
        }
    }

    #[test]
    fn musl_host_does_not_select_glibc_llvm_asset() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Linux
            || crate::platform::host::facts::libc() != crate::platform::host::facts::HostLibc::Musl
        {
            return;
        }
        assert!(
            host_triple_for_llvm().is_none(),
            "musl hosts must not select the glibc LLVM archive",
        );
    }
}
