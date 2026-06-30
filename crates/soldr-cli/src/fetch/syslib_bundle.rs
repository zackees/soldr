//! Shared materializer for soldr-toolchain syslib bundles.
//!
//! The forge ingest path publishes rows like
//! `zstd/1.5.7/linux-x64-musl/bundle.tar.zst` into the flat v1
//! catalogue. Those rows intentionally reuse the same asset filename
//! (`bundle.tar.zst`) across target shapes, so consumers must resolve
//! them by exact URL rather than `(owner, repo, tag, asset)`.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

use super::github::http_client;
use super::manifest_lookup;
use super::trust;

const TOOLCHAIN_ASSETS_BASE_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets";
const SYSROOT_ASSET_NAME: &str = "bundle.tar.zst";

pub(crate) fn asset_url_for(tool: &str, version: &str, slug: &str) -> String {
    format!("{TOOLCHAIN_ASSETS_BASE_URL}/{tool}/{version}/{slug}/{SYSROOT_ASSET_NAME}")
}

pub(crate) async fn ensure_syslib_bundle(
    paths: &SoldrPaths,
    target_triple: &str,
    tool: &str,
    version: &str,
    slug: &str,
    expected_files: &[&str],
) -> Result<PathBuf, SoldrError> {
    let url = asset_url_for(tool, version, slug);
    let manifest = manifest_lookup::get_or_fetch().await;
    let entry = manifest.lookup_url(&url).ok_or_else(|| {
        SoldrError::Other(format!(
            "{tool} sysroot for {target_triple} ({slug}) is not yet ingested into the \
             soldr-toolchain catalogue. Expected URL: {url}\n\
             Tracking: https://github.com/zackees/soldr/issues/1064"
        ))
    })?;
    let expected_sha256 = entry.sha256.trim().to_ascii_lowercase();
    let entry_url = entry.url.clone();

    paths.ensure_dirs()?;
    let install_dir = install_dir_for(paths, target_triple, tool, version, slug);
    let stamp = install_dir.join(".complete");
    let sysroot = package_root_for(&install_dir);

    if stamp_matches(&stamp, &expected_sha256) && expected_files_present(&sysroot, expected_files) {
        return Ok(sysroot);
    }

    eprintln!("soldr: fetching {tool} sysroot v{version} for {target_triple} from {entry_url}...");
    let client = http_client()?;
    let resp = client
        .get(&entry_url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "{tool} sysroot download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    let digest = trust::sha256_of(&bytes);
    if digest != expected_sha256 {
        return Err(SoldrError::Other(format!(
            "{tool} sysroot sha256 mismatch for {target_triple}: expected {expected_sha256}, \
             got {digest} (catalogue blob may have been replaced; refusing to extract)"
        )));
    }
    eprintln!(
        "soldr: trust: manifest-verified {tool} sysroot v{version} for {target_triple} sha256={digest}"
    );

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    std::fs::create_dir_all(&install_dir)?;
    extract_tar_zst_tree(&bytes, &install_dir)?;

    let sysroot = package_root_for(&install_dir);
    ensure_expected_files(&sysroot, expected_files, tool, target_triple)?;
    write_stamp(&stamp, version, &entry_url, &expected_sha256)?;
    eprintln!(
        "soldr: extracted {tool} sysroot for {target_triple} to {}",
        sysroot.display()
    );
    Ok(sysroot)
}

fn install_dir_for(
    paths: &SoldrPaths,
    target_triple: &str,
    tool: &str,
    version: &str,
    slug: &str,
) -> PathBuf {
    paths
        .root
        .join("sdk")
        .join(target_triple)
        .join(tool)
        .join(version)
        .join(slug)
}

fn package_root_for(install_dir: &Path) -> PathBuf {
    let package = install_dir.join("package");
    if package.is_dir() {
        package
    } else {
        install_dir.to_path_buf()
    }
}

fn expected_files_present(sysroot: &Path, expected_files: &[&str]) -> bool {
    expected_files.iter().all(|rel| sysroot.join(rel).is_file())
}

fn ensure_expected_files(
    sysroot: &Path,
    expected_files: &[&str],
    tool: &str,
    target_triple: &str,
) -> Result<(), SoldrError> {
    let missing = expected_files
        .iter()
        .filter(|rel| !sysroot.join(rel).is_file())
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(SoldrError::Archive(format!(
        "{tool} sysroot extract for {target_triple} is missing expected files under {}: {}",
        sysroot.display(),
        missing.join(", ")
    )))
}

fn stamp_matches(stamp: &Path, expected_sha256: &str) -> bool {
    std::fs::read_to_string(stamp)
        .map(|text| {
            text.lines()
                .any(|line| line == format!("sha256={expected_sha256}"))
        })
        .unwrap_or(false)
}

fn write_stamp(stamp: &Path, version: &str, url: &str, sha256: &str) -> Result<(), SoldrError> {
    std::fs::write(
        stamp,
        format!("version={version}\nurl={url}\nsha256={sha256}\n"),
    )?;
    Ok(())
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

    crate::timed_test!(asset_url_uses_top_level_tool_layout, {
        let url = asset_url_for("zstd", "1.5.7", "linux-x64-musl");
        assert_eq!(
            url,
            "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/zstd/1.5.7/linux-x64-musl/bundle.tar.zst"
        );
        assert!(!url.contains("/deps/"));
    });

    crate::timed_test!(package_root_prefers_inner_package_dir, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path().join("install");
        std::fs::create_dir_all(root.join("package")).expect("mkdir");
        assert_eq!(package_root_for(&root), root.join("package"));
    });

    crate::timed_test!(cache_hit_requires_matching_stamp_and_expected_files, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let install = install_dir_for(
            &paths,
            "x86_64-unknown-linux-musl",
            "zstd",
            "1.5.7",
            "linux-x64-musl",
        );
        let sysroot = install.join("package");
        std::fs::create_dir_all(sysroot.join("include")).expect("include");
        std::fs::create_dir_all(sysroot.join("lib").join("pkgconfig")).expect("pkgconfig");
        std::fs::write(sysroot.join("include").join("zstd.h"), b"").expect("zstd.h");
        std::fs::write(
            sysroot.join("lib").join("pkgconfig").join("libzstd.pc"),
            b"",
        )
        .expect("pc");
        let stamp = install.join(".complete");
        let sha = "0".repeat(64);
        write_stamp(
            &stamp,
            "1.5.7",
            "https://example.invalid/bundle.tar.zst",
            &sha,
        )
        .expect("stamp");

        assert!(stamp_matches(&stamp, &sha));
        assert!(expected_files_present(
            &package_root_for(&install),
            &["include/zstd.h", "lib/pkgconfig/libzstd.pc"]
        ));
        assert!(!expected_files_present(
            &package_root_for(&install),
            &["include/zstd.h", "lib/pkgconfig/missing.pc"]
        ));
    });
}
