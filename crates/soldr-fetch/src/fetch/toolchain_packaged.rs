//! Resolution for tools repackaged by soldr-toolchain across every target.

use super::{
    archive, check_cache, current_unix_ms, manifest_lookup, smoke_test_or_evict, FetchResult,
};
use crate::core::{SoldrError, SoldrPaths, TargetTriple};

/// Exact filename for binaries that soldr-toolchain republishes across all
/// eight supported targets. Upstream Dylint 6.0.3 only publishes Linux GNU;
/// these explicitly opted-in packages keep the binary-only contract intact on
/// Windows, macOS, and musl without weakening catalogue SHA verification.
pub(super) fn asset_name(cache_name: &str, version: &str, target: &TargetTriple) -> Option<String> {
    matches!(cache_name, "cargo-dylint" | "dylint-link").then(|| {
        format!(
            "{}-{}-{}.tar.gz",
            cache_name,
            version.trim_start_matches('v'),
            target.triple()
        )
    })
}

/// Resolve an explicitly supported soldr-toolchain repackaged binary by its
/// exact versioned filename. The catalogue owner is intentionally not the
/// upstream repository: these rows are produced and hosted by
/// `zackees/soldr-toolchain`, and a unique exact filename plus its SHA-256 pin
/// is the complete identity needed by this path.
pub(super) async fn try_binary(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    version: &str,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let Some(asset_name) = asset_name(cache_name, version, target) else {
        return Ok(None);
    };
    let manifest = manifest_lookup::get_or_fetch().await;
    let matches = manifest.lookup_asset(&asset_name);
    if matches.is_empty() {
        return Ok(None);
    }
    if matches.len() != 1 {
        return Err(SoldrError::Other(format!(
            "toolchain catalogue has {} rows for exact asset {asset_name}",
            matches.len()
        )));
    }
    let entry = matches[0];
    let bare_version = version.trim_start_matches('v');
    if let Some(result) = check_cache(paths, cache_name, bare_version, binary_names, target)? {
        return Ok(Some(result));
    }

    eprintln!(
        "soldr: toolchain catalogue hit for {} v{} {} -> {}",
        cache_name,
        bare_version,
        target.triple(),
        entry.asset
    );
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let binary_path = archive::download_and_extract_with_pin(
        paths,
        cache_name,
        bare_version,
        &entry.url,
        target,
        binary_names,
        Some((&entry.asset, &entry.sha256)),
    )
    .await?;
    smoke_test_or_evict(&binary_path, cache_name, target)?;
    soldr_core::build_log_meta::fetch_timing::record(
        soldr_core::build_log_meta::fetch_timing::FetchTiming {
            name: cache_name.to_string(),
            source: "catalogue".to_string(),
            started_at_ms: download_started_at_ms,
            duration_ms: download_started.elapsed().as_millis() as u64,
        },
    );

    Ok(Some(FetchResult {
        binary_path,
        version: bare_version.to_string(),
        cached: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(dylint_packages_cover_all_supported_targets, {
        let targets = [
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
        ];
        for triple in targets {
            let target = TargetTriple::from_triple(triple).unwrap();
            assert_eq!(
                asset_name("cargo-dylint", "v6.0.3", &target),
                Some(format!("cargo-dylint-6.0.3-{triple}.tar.gz"))
            );
            assert_eq!(
                asset_name("dylint-link", "6.0.3", &target),
                Some(format!("dylint-link-6.0.3-{triple}.tar.gz"))
            );
        }
        let host = TargetTriple::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(asset_name("cargo-nextest", "1", &host), None);
    });

    crate::timed_test!(catalogued_dylint_binary_is_smoked_and_evicted, {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("cargo-dylint");
        std::fs::write(&bogus, b"not an executable").unwrap();
        let target = TargetTriple::host().unwrap();

        let error = smoke_test_or_evict(&bogus, "cargo-dylint", &target).unwrap_err();

        assert!(error.to_string().contains("smoke test failed"));
        assert!(!bogus.exists(), "failed catalogue binary must be evicted");
    });
}
