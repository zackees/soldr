//! Shared download + extract for soldr#1064 Phase B `*-sys` C library
//! catalogue distribution.
//!
//! Each `<lib>_sysroot.rs` consumer module calls
//! [`ensure_syslib_bundle`] with its own `(lib_name, version, slug)`
//! tuple. The helper:
//!
//! 1. Resolves the catalogue URL via [`asset_url_for`].
//! 2. Looks up the matching `sha256` in the v1 toolchain catalogue
//!    (already cached process-wide by [`super::manifest_lookup`]).
//!    If the catalogue doesn't list the URL yet, the helper returns
//!    the existing "not yet ingested" error so callers fall through
//!    to the crate's vendored compile, exactly like the original
//!    `openssl_sysroot.rs` stub did.
//! 3. Downloads + sha256-verifies + extracts the `tar.zst` bundle
//!    into `~/.soldr/sdk/syslib/<lib>/<version>/<slug>/` under a
//!    `.complete` sentinel file. Re-invocations after a successful
//!    extract are a stat call.
//! 4. Returns the sysroot root path (the directory containing
//!    `package/` from the forge artifact, lifted up so callers see
//!    `lib/` + `include/` + `lib/pkgconfig/` directly).
//!
//! Why a shared helper instead of inlining in each consumer module?
//! The six `*-sys` libs ship the exact same payload shape (forge
//! produces `package/lib/...`, `package/include/...`,
//! `package/lib/pkgconfig/...` for every one). Hard-coding the
//! same flow six times would just be six copies of the same bug
//! site.

use std::path::{Path, PathBuf};

use super::manifest_lookup;
use super::stream_download::{
    asset_http_client_with_protocol, get_request, send_asset_request, stream_response_to_temp_file,
    AssetProtocol, DownloadedAsset, ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};
use crate::core::{SoldrError, SoldrPaths};

pub const SYSLIB_ASSET_ORIGIN_ENV_VAR: &str = "SOLDR_SYSLIB_ASSET_ORIGIN";
const DEFAULT_SYSLIB_ASSET_ORIGIN: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets";

/// Build the canonical assets-branch URL for a `(lib, version, slug)`
/// tuple. Mirrors the layout `forge_to_catalogue.py` writes:
///
/// ```text
/// https://media.githubusercontent.com/media/zackees/soldr-toolchain/
///   assets/<lib>/<version>/<slug>/bundle.tar.zst
/// ```
pub fn asset_url_for(lib: &str, version: &str, slug: &str) -> String {
    let origin = std::env::var(SYSLIB_ASSET_ORIGIN_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SYSLIB_ASSET_ORIGIN.to_string());
    format!(
        "{}/{lib}/{version}/{slug}/bundle.tar.zst",
        origin.trim_end_matches('/')
    )
}

/// Materialize a syslib bundle on disk and return the sysroot root.
///
/// The returned path is what `blessed_build::prepare`'s env injection
/// expects: the directory whose children are `lib/`, `include/`, and
/// (for pkg-config-style libs) `lib/pkgconfig/`.
pub async fn ensure_syslib_bundle(
    paths: &SoldrPaths,
    lib: &str,
    version: &str,
    slug: &str,
) -> Result<PathBuf, SoldrError> {
    paths.ensure_dirs()?;
    let url = asset_url_for(lib, version, slug);

    let install_root = paths.bin.join("syslib").join(lib).join(version).join(slug);
    let sysroot = install_root.join("package");
    let stamp = install_root.join(".complete");

    if stamp.is_file() && sysroot.is_dir() {
        return Ok(sysroot);
    }

    // Cross-process install guard: cargo fans out build scripts and CI
    // fans out jobs, so several soldr processes can miss the stamp
    // simultaneously and race the remove_dir_all + extract below.
    // Blocking exclusive lock — the winner installs, waiters re-check
    // the stamp after acquiring and short-circuit. Held (via the
    // returned handle) until this function returns.
    let _install_lock = acquire_install_lock(
        &paths.bin.join("syslib"),
        &format!("{lib}-{version}-{slug}"),
    )?;
    if stamp.is_file() && sysroot.is_dir() {
        return Ok(sysroot);
    }

    // Resolve sha256 from the toolchain catalogue. The catalogue is
    // process-cached; the first call inside soldr's run fetches the
    // document, subsequent ones hit the OnceLock.
    let entry = match catalogue_entry_for_url(&url).await {
        Some(entry) => entry,
        None => {
            // soldr#2132 item 4: two very different causes used to produce the
            // same message. "Not yet ingested" is a real state, but so is "the
            // catalogue never loaded", and blaming ingestion for a network
            // failure sends the reader to the wrong repository.
            return Err(SoldrError::Other(missing_catalogue_entry_message(
                lib,
                version,
                slug,
                &url,
                manifest_lookup::get_or_fetch().await.entries.is_empty(),
            )));
        }
    };
    let expected_sha256 = entry.sha256.clone();

    if !crate::core::quiet::diagnostics_suppressed() {
        eprintln!("soldr: fetching syslib {lib}/{version}/{slug} from {url}...");
    }

    // soldr#2132: retry the download itself. A truncated body here surfaced as
    // `managed cmake unavailable ... network error: error decoding response
    // body` and then, a hundred log lines later, as `can't find crate for
    // std` -- an error naming the wrong thing entirely.
    let downloaded =
        super::retry::with_asset_backoff(&format!("syslib {lib}/{version}/{slug}"), || {
            download_syslib_bundle(&url)
        })
        .await?;

    let digest = downloaded.sha256();
    if digest != expected_sha256 {
        return Err(SoldrError::Other(format!(
            "syslib bundle sha256 mismatch for {lib}/{version}/{slug}: \
             expected {expected_sha256}, got {digest}"
        )));
    }
    if !crate::core::quiet::diagnostics_suppressed() {
        eprintln!("soldr: trust: verified syslib {lib}/{version}/{slug} sha256={digest}");
    }

    if install_root.exists() {
        std::fs::remove_dir_all(&install_root)?;
    }
    std::fs::create_dir_all(&install_root)?;
    extract_tar_zst(std::fs::File::open(downloaded.path())?, &install_root)?;

    if !sysroot.is_dir() {
        return Err(SoldrError::Archive(format!(
            "syslib extract for {lib}/{version}/{slug} did not produce expected \
             {} (forge artifact layout drift?)",
            sysroot.display()
        )));
    }

    std::fs::write(&stamp, format!("{lib} {version} {slug}"))?;
    if !crate::core::quiet::diagnostics_suppressed() {
        eprintln!("soldr: extracted syslib to {}", sysroot.display());
    }
    Ok(sysroot)
}

/// One attempt at downloading a syslib bundle. Every failure mode here is
/// [`SoldrError::Network`], which is exactly what [`super::retry`] retries;
/// sha256 verification is deliberately *outside* this function so an integrity
/// failure stays fatal on the first try.
async fn download_syslib_bundle(url: &str) -> Result<DownloadedAsset, SoldrError> {
    let client =
        asset_http_client_with_protocol("a *-sys catalogue bundle", AssetProtocol::Http1Only)?;
    let resp = send_asset_request(
        get_request(&client, url).header(reqwest::header::ACCEPT_ENCODING, "identity"),
        url,
        ASSET_HEADER_TIMEOUT,
    )
    .await?;
    stream_response_to_temp_file(resp, url, ASSET_IDLE_TIMEOUT).await
}

/// Acquire a **blocking** exclusive cross-process lock for an install
/// keyed by `key`, creating `lock_dir` if needed. The lock lives in a
/// dotfile next to the install tree and releases when the returned
/// handle drops. Callers MUST re-check their completion stamp after
/// acquiring — a waiter usually finds the winner already finished.
///
/// Blocking (not `try_lock`) on purpose: the right behavior for a
/// second `soldr` process is to wait for the first install to finish
/// and then reuse it, not to fail or duplicate the download.
pub(crate) fn acquire_install_lock(
    lock_dir: &Path,
    key: &str,
) -> Result<std::fs::File, SoldrError> {
    use fs2::FileExt;
    std::fs::create_dir_all(lock_dir)?;
    let lock_path = lock_dir.join(format!(".{key}.lock"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Look up a catalogue entry whose `url` field exactly matches `url`.
/// The v1 catalogue is keyed by `(owner, repo, tag, asset)` but the
/// asset names for our bundles collide across libs (`bundle.tar.zst`),
/// so we filter by URL substring instead. Returns `None` when the
/// catalogue is empty (network failure / disabled) or the URL hasn't
/// been ingested yet.
async fn catalogue_entry_for_url(url: &str) -> Option<manifest_lookup::ManifestEntry> {
    let index = manifest_lookup::get_or_fetch().await;
    index.entries.iter().find(|e| e.url == url).cloned()
}

/// The error for a syslib bundle with no catalogue entry.
///
/// soldr#2132 item 4. `catalogue_empty` distinguishes the two causes that used
/// to share one message: an index that loaded but does not list this asset
/// (a genuine ingestion gap) versus an index that never loaded at all (a
/// network failure several steps earlier, already warned about by
/// `manifest_lookup::get_or_fetch`).
fn missing_catalogue_entry_message(
    lib: &str,
    version: &str,
    slug: &str,
    url: &str,
    catalogue_empty: bool,
) -> String {
    if catalogue_empty {
        format!(
            "syslib bundle for {lib}/{version}/{slug} cannot be resolved because the \
             soldr-toolchain catalogue is empty -- it failed to load earlier in this \
             run (see the warning above). This is a fetch failure, not a missing \
             asset. Expected URL: {url}"
        )
    } else {
        format!(
            "syslib bundle for {lib}/{version}/{slug} not yet ingested into the \
             soldr-toolchain catalogue. Expected URL: {url}\n\
             Track: https://github.com/zackees/soldr/issues/1064"
        )
    }
}

fn extract_tar_zst<R: std::io::Read>(reader: R, dest: &Path) -> Result<(), SoldrError> {
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    // soldr#2300: symlink-aware unpack — on Windows, dir symlinks such as
    // the GNU/Linux sysroot's `usr/lib -> lib64` must be created with the
    // directory flavor (or copied) to be traversable.
    super::tar_extract::unpack_tar(&mut archive, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn asset_url_layout() {
        let u = asset_url_for("zstd", "1.5.7", "linux-x64-gnu");
        assert!(
            u.contains("/zstd/1.5.7/linux-x64-gnu/bundle.tar.zst"),
            "{u}"
        );
        assert!(u.starts_with("https://media.githubusercontent.com/"));
    }

    #[test]
    fn slug_distinct_per_target() {
        let a = asset_url_for("sqlite", "3.46.0", "windows-x64");
        let b = asset_url_for("sqlite", "3.46.0", "linux-arm64-musl");
        assert_ne!(a, b);
    }

    #[test]
    fn large_binary_download_uses_shared_idle_timeout() {
        assert_eq!(
            super::super::stream_download::ASSET_IDLE_TIMEOUT,
            Duration::from_secs(120)
        );
    }

    #[test]
    fn install_lock_serializes_racing_threads() {
        // 8 threads race the same install key and each performs a
        // deliberately non-atomic read-modify-write on a shared file
        // inside the critical section. Without mutual exclusion the
        // lost-update race makes the final count < 8; with the lock
        // it is exactly 8. (fs2 advisory locks are per-handle, so
        // same-process threads contend just like separate processes.)
        let tmp = tempfile::tempdir().expect("tmpdir");
        let lock_dir = tmp.path().to_path_buf();
        let counter = tmp.path().join("counter");
        std::fs::write(&counter, "0").unwrap();

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let lock_dir = lock_dir.clone();
                let counter = counter.clone();
                std::thread::spawn(move || {
                    let _lock = acquire_install_lock(&lock_dir, "race-key").expect("lock");
                    let n: u32 = std::fs::read_to_string(&counter)
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    // Widen the race window so a broken lock actually
                    // loses updates rather than passing by luck.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    std::fs::write(&counter, format!("{}", n + 1)).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        let final_count: u32 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(final_count, 8, "install lock must serialize writers");
    }

    #[test]
    fn missing_entry_message_names_the_real_cause() {
        // An index that loaded but does not list the asset: a genuine
        // ingestion gap, and the tracking issue is the right pointer.
        let ingestion_gap =
            missing_catalogue_entry_message("zstd", "1.5.6", "linux-x64-gnu", "https://x/y", false);
        assert!(
            ingestion_gap.contains("not yet ingested"),
            "expected the ingestion wording: {ingestion_gap}"
        );
        assert!(ingestion_gap.contains("issues/1064"));

        // An index that never loaded: a fetch failure several steps earlier.
        // Blaming ingestion here sends the reader to the wrong repository,
        // which is what soldr#2132 item 4 is about.
        let never_loaded =
            missing_catalogue_entry_message("zstd", "1.5.6", "linux-x64-gnu", "https://x/y", true);
        assert!(
            never_loaded.contains("catalogue is empty"),
            "expected the fetch-failure wording: {never_loaded}"
        );
        assert!(
            never_loaded.contains("not a missing asset"),
            "must say plainly that the asset is not the problem: {never_loaded}"
        );
        assert!(
            !never_loaded.contains("not yet ingested"),
            "must NOT blame ingestion for a fetch failure: {never_loaded}"
        );

        // Both name the asset, so the message is actionable either way.
        for message in [&ingestion_gap, &never_loaded] {
            assert!(message.contains("zstd/1.5.6/linux-x64-gnu"), "{message}");
        }
    }
}
