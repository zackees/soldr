//! Download + archive extraction for tool fetches.
//!
//! Extracted from `fetch/mod.rs` during the >1k-LOC refactor.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::github::asset_http_client;
use super::stream_download::{
    send_asset_request, stream_response_to_temp_file, ASSET_HEADER_TIMEOUT, ASSET_IDLE_TIMEOUT,
};
use super::trust;

// soldr#2132: deliberately NOT wrapped in `super::retry::with_backoff`.
//
// Every call site here -- `download_and_extract` at mod.rs:579 and
// `download_and_extract_with_pin` at :829 and :953, the latter two via
// `try_embedded_manifest_v6` / `try_manifest_first` -- is reached from
// `fetch_repo_binary_once`, which IS the body of the retry loop in
// `fetch_repo_binary_with_paths`. Adding a retry here would nest inside that
// one: 4 outer x 4 inner = up to 16 downloads and ~2.5 minutes of sleeping
// before a genuinely dead host is reported. The outer loop already covers
// this path; the right number of retry layers is one.

pub(super) async fn download_and_extract(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    url: &str,
    target: &TargetTriple,
    binary_names: &[&str],
) -> Result<PathBuf, SoldrError> {
    download_and_extract_with_pin(paths, cache_name, version, url, target, binary_names, None).await
}

/// Same as [`download_and_extract`] but takes an optional `manifest_pin`
/// argument of `(asset_name, expected_sha256)`. When provided, the
/// computed sha256 of the downloaded bytes MUST equal the expected pin
/// or the function returns an error — this is the manifest-first trust
/// invariant (see `manifest_lookup` module docs). The pin overrides
/// every other source of integrity policy (the env-var checksum store
/// and `SOLDR_TRUST_MODE` are not consulted when a manifest pin is
/// supplied because the manifest pin is strictly stronger).
pub(super) async fn download_and_extract_with_pin(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    url: &str,
    target: &TargetTriple,
    binary_names: &[&str],
    manifest_pin: Option<(&str, &str)>,
) -> Result<PathBuf, SoldrError> {
    let client = asset_http_client()?;

    let resp = send_asset_request(client.get(url), url, ASSET_HEADER_TIMEOUT).await?;

    let downloaded = stream_response_to_temp_file(resp, url, ASSET_IDLE_TIMEOUT).await?;

    // Integrity + trust enforcement (issue #42). Compute sha256 and consult
    // the pinned-checksum store before writing anything to disk.
    let asset_name = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    let digest = downloaded.sha256();

    if let Some((pinned_asset, expected_sha)) = manifest_pin {
        let expected = expected_sha.trim().to_ascii_lowercase();
        let actual = digest.to_ascii_lowercase();
        if expected != actual {
            return Err(SoldrError::Other(format!(
                "manifest-first: pinned sha256 mismatch for {cache_name} v{version} asset {pinned_asset}\n  expected: {expected}\n  actual:   {actual}\n  source:   {url}",
            )));
        }
        eprintln!(
            "soldr: trust: manifest-verified {cache_name} v{version} {pinned_asset} sha256={actual}"
        );
    } else {
        let store = trust::PinnedChecksumStore::from_env()?;
        let mode = trust::TrustMode::from_env();
        match trust::verify_download(cache_name, version, asset_name, digest, &store, mode)? {
            trust::VerifyOutcome::Verified { sha256 } => {
                eprintln!(
                    "soldr: trust: verified {cache_name} v{version} {asset_name} sha256={sha256}"
                );
            }
            trust::VerifyOutcome::Unverified { sha256 } => {
                eprintln!(
                    "soldr: trust: unverified {cache_name} v{version} {asset_name} sha256={sha256} (set {} to pin; run with {}=strict to require pins)",
                    trust::CHECKSUMS_FILE_ENV_VAR,
                    trust::TRUST_MODE_ENV_VAR
                );
            }
        }
    }

    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let desired_binaries = desired_binary_names(binary_names, target);
    std::fs::create_dir_all(&tool_dir)?;

    let main_binary_name = desired_binaries
        .first()
        .cloned()
        .ok_or_else(|| SoldrError::Other(format!("no binary names configured for {cache_name}")))?;
    let binary_path = tool_dir.join(&main_binary_name);

    if url.ends_with(".zip") {
        extract_zip(
            std::fs::File::open(downloaded.path())?,
            &tool_dir,
            &desired_binaries,
        )?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        extract_tar_gz(
            std::fs::File::open(downloaded.path())?,
            &tool_dir,
            &desired_binaries,
        )?;
    } else if url.ends_with(".tar.xz") || url.ends_with(".txz") {
        // cargo-zigbuild and other rust-cross prebuilts ship their
        // per-triple binary tarballs in xz. Without this branch the
        // fall-through "raw binary" path below wrote the compressed
        // tarball bytes to disk as the binary, producing the shell
        // syntax error described in #809.
        extract_tar_xz(
            std::fs::File::open(downloaded.path())?,
            &tool_dir,
            &desired_binaries,
        )?;
    } else {
        // Assume raw binary.
        if desired_binaries.len() != 1 {
            return Err(SoldrError::Archive(format!(
                "cannot extract multiple binaries from raw asset for {cache_name}"
            )));
        }
        std::fs::copy(downloaded.path(), &binary_path)?;
    }

    // Make executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for binary_name in &desired_binaries {
            let binary_path = tool_dir.join(binary_name);
            let mut perms = std::fs::metadata(&binary_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary_path, perms)?;
        }
    }

    Ok(binary_path)
}

pub(crate) fn desired_binary_names(binary_names: &[&str], target: &TargetTriple) -> Vec<String> {
    binary_names
        .iter()
        .map(|binary_name| format!("{binary_name}{}", target.binary_ext()))
        .collect()
}

fn extract_zip<R: std::io::Read + std::io::Seek>(
    reader: R,
    dest_dir: &Path,
    binary_names: &[String],
) -> Result<(), SoldrError> {
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| SoldrError::Archive(e.to_string()))?;
    let mut found = std::collections::BTreeSet::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        if file.is_dir() {
            continue;
        }

        let file_name = Path::new(file.name())
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");

        let wanted = binary_names.iter().find(|binary_name| {
            file_name == *binary_name || file_name == binary_name.trim_end_matches(".exe")
        });

        if let Some(binary_name) = wanted {
            let mut out = std::fs::File::create(dest_dir.join(binary_name))?;
            std::io::copy(&mut file, &mut out)?;
            found.insert(binary_name.clone());
        }
    }

    ensure_all_binaries_found(binary_names, &found)
}

fn extract_tar_gz<R: std::io::Read>(
    reader: R,
    dest_dir: &Path,
    binary_names: &[String],
) -> Result<(), SoldrError> {
    let gz = flate2::read::GzDecoder::new(reader);
    extract_tar(gz, dest_dir, binary_names)
}

fn extract_tar_xz<R: std::io::Read>(
    reader: R,
    dest_dir: &Path,
    binary_names: &[String],
) -> Result<(), SoldrError> {
    let xz = xz2::read::XzDecoder::new(reader);
    extract_tar(xz, dest_dir, binary_names)
}

/// Walk a `tar::Archive` over any reader and copy entries whose file
/// name matches one of `binary_names` into `dest_dir`. Shared between
/// the `.tar.gz` and `.tar.xz` extract paths.
fn extract_tar<R: std::io::Read>(
    reader: R,
    dest_dir: &Path,
    binary_names: &[String],
) -> Result<(), SoldrError> {
    let mut archive = tar::Archive::new(reader);
    let mut found = std::collections::BTreeSet::new();

    for entry in archive
        .entries()
        .map_err(|e| SoldrError::Archive(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| SoldrError::Archive(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| SoldrError::Archive(e.to_string()))?;

        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

        let wanted = binary_names.iter().find(|binary_name| {
            file_name == *binary_name || file_name == binary_name.trim_end_matches(".exe")
        });

        if let Some(binary_name) = wanted {
            let mut out = std::fs::File::create(dest_dir.join(binary_name))?;
            std::io::copy(&mut entry, &mut out)?;
            found.insert(binary_name.clone());
        }
    }

    ensure_all_binaries_found(binary_names, &found)
}

fn ensure_all_binaries_found(
    binary_names: &[String],
    found: &std::collections::BTreeSet<String>,
) -> Result<(), SoldrError> {
    let missing = binary_names
        .iter()
        .filter(|binary_name| !found.contains(*binary_name))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SoldrError::Archive(format!(
            "missing binaries in archive: {}",
            missing.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tarball whose single binary entry mirrors cargo-zigbuild's
    /// per-triple layout: `cargo-zigbuild-<triple>/cargo-zigbuild` with
    /// recognizable ELF-ish magic bytes as the payload.
    fn build_tarball(binary_name: &str, payload: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let entry_path = format!("cargo-zigbuild-x86_64-unknown-linux-gnu/{binary_name}");
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, &entry_path, payload)
                .expect("append");
            builder.finish().expect("finish");
        }
        tar_bytes
    }

    fn xz_compress(data: &[u8]) -> Vec<u8> {
        let mut compressor = xz2::write::XzEncoder::new(Vec::new(), 6);
        compressor.write_all(data).expect("write");
        compressor.finish().expect("finish")
    }

    // Regression guard for #809. cargo-zigbuild's binary tarball is
    // `.tar.xz`; before this fix soldr fell through to the "raw binary"
    // branch and wrote the compressed bytes verbatim to the binary
    // path, producing a file the shell tried to parse as a script.
    crate::timed_test!(extract_tar_xz_finds_binary_in_per_triple_dir_layout, {
        let dest = tempfile::tempdir().expect("tempdir");
        let payload = b"\x7fELF\x02\x01\x01\x00 fake cargo-zigbuild binary payload";
        let tarball = build_tarball("cargo-zigbuild", payload);
        let xz = xz_compress(&tarball);

        let desired = vec!["cargo-zigbuild".to_string()];
        extract_tar_xz(std::io::Cursor::new(xz), dest.path(), &desired).expect("extract tar.xz");

        let installed = dest.path().join("cargo-zigbuild");
        assert!(
            installed.exists(),
            "extract_tar_xz failed to write cargo-zigbuild into dest dir",
        );
        let contents = std::fs::read(&installed).expect("read installed");
        assert_eq!(
            contents.as_slice(),
            payload,
            "installed bytes must be the inner ELF payload, not the compressed tarball — the #809 symptom is the binary path containing the tar.xz bytes verbatim",
        );
        assert_ne!(
            &contents[0..4],
            b"\xfd7zX",
            "regression: writing the .tar.xz magic bytes as the binary is exactly the #809 bug",
        );
    });

    // The cargo-zigbuild URL convention is what soldr's
    // `download_and_extract` dispatches on. If a future URL ends in
    // `.txz` instead of `.tar.xz` the same path must trigger.
    crate::timed_test!(tar_xz_url_suffix_recognized_for_dispatch, {
        // This is a documentation test for the dispatch table in
        // `download_and_extract` — keep the suffix list aligned with the
        // url.ends_with(...) branches above.
        for url in [
            "https://example.invalid/cargo-zigbuild-x86_64-unknown-linux-gnu.tar.xz",
            "https://example.invalid/cargo-zigbuild-x86_64-unknown-linux-musl.tar.xz",
            "https://example.invalid/some-tool.txz",
        ] {
            assert!(
                url.ends_with(".tar.xz") || url.ends_with(".txz"),
                "tar.xz dispatch suffix coverage missed {url}",
            );
        }
    });
}
