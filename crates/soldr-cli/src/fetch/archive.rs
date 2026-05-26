//! Download + archive extraction for tool fetches.
//!
//! Extracted from `fetch/mod.rs` during the >1k-LOC refactor.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::github::http_client;
use super::trust;

pub(super) async fn download_and_extract(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    url: &str,
    target: &TargetTriple,
    binary_names: &[&str],
) -> Result<PathBuf, SoldrError> {
    let client = http_client()?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "download failed: HTTP {}",
            resp.status()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    // Integrity + trust enforcement (issue #42). Compute sha256 and consult
    // the pinned-checksum store before writing anything to disk.
    let asset_name = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    let digest = trust::sha256_of(&bytes);
    let store = trust::PinnedChecksumStore::from_env()?;
    let mode = trust::TrustMode::from_env();
    match trust::verify_download(cache_name, version, asset_name, &digest, &store, mode)? {
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

    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let desired_binaries = desired_binary_names(binary_names, target);
    std::fs::create_dir_all(&tool_dir)?;

    let main_binary_name = desired_binaries
        .first()
        .cloned()
        .ok_or_else(|| SoldrError::Other(format!("no binary names configured for {cache_name}")))?;
    let binary_path = tool_dir.join(&main_binary_name);

    if url.ends_with(".zip") {
        extract_zip(&bytes, &tool_dir, &desired_binaries)?;
    } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
        extract_tar_gz(&bytes, &tool_dir, &desired_binaries)?;
    } else {
        // Assume raw binary.
        if desired_binaries.len() != 1 {
            return Err(SoldrError::Archive(format!(
                "cannot extract multiple binaries from raw asset for {cache_name}"
            )));
        }
        std::fs::write(&binary_path, &bytes)?;
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

fn extract_zip(data: &[u8], dest_dir: &Path, binary_names: &[String]) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
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

fn extract_tar_gz(data: &[u8], dest_dir: &Path, binary_names: &[String]) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
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
