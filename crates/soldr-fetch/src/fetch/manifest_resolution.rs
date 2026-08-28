async fn try_manifest_first(
    paths: &SoldrPaths,
    cache_name: &str,
    binary_names: &[&str],
    repo: &github::RepoInfo,
    tag: &str,
    tag_prefix: Option<&str>,
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let manifest = manifest_lookup::get_or_fetch().await;
    if manifest.fail_closed {
        return Err(SoldrError::Other(
            "catalogue.v2.json was present but invalid; refusing legacy/live fallback".into(),
        ));
    }
    let candidates = manifest.lookup_release(&repo.owner, &repo.repo, tag);
    // Tag normalization: known_tools may resolve a tag prefix like
    // `cargo-audit/v0.21.0`, so try the bare tag first and the
    // composed tag if nothing matched. Mirrors the candidate-tag
    // expansion `github::fetch_release` does over the live API.
    let candidates_fallback;
    let candidates = if candidates.is_empty() {
        let prefixed = match tag_prefix {
            Some(prefix) => format!("{prefix}{}", tag.trim_start_matches('v')),
            None => return Ok(None),
        };
        candidates_fallback = manifest.lookup_release(&repo.owner, &repo.repo, &prefixed);
        if candidates_fallback.is_empty() {
            return Ok(None);
        }
        candidates_fallback.to_vec()
    } else {
        candidates
    };

    // The asset matcher reuses the existing platform-keyword scoring
    // logic. We synthesize a one-off `AssetInfo` list from the manifest
    // entries and let the binary-aware matcher pick the best fit for our
    // requested program and target.
    let asset_infos: Vec<github::AssetInfo> = candidates
        .iter()
        .map(|entry| github::AssetInfo {
            name: entry.asset.clone(),
            download_url: entry.transport.direct_url().unwrap_or_default().to_string(),
        })
        .collect();
    let asset = match github::match_asset_for_binaries(&asset_infos, target, binary_names) {
        Ok(asset) => asset,
        Err(_) => return Ok(None),
    };
    if !github::asset_name_matches_any_binary(&asset.name, binary_names) {
        return Ok(None);
    }
    let matched_entry = candidates
        .iter()
        .find(|e| e.asset == asset.name)
        .copied()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "manifest-first: matched asset {} but could not find its entry",
                asset.name
            ))
        })?;

    // Tag → version conversion: trim a leading `v` and any monorepo
    // prefix the asset is filed under, to match the layout the live
    // path uses for `~/.soldr/bin/<name>-<version>/`.
    let version = match tag_prefix {
        Some(prefix) => matched_entry
            .tag
            .strip_prefix(prefix)
            .unwrap_or(&matched_entry.tag),
        None => matched_entry.tag.as_str(),
    }
    .trim_start_matches('v')
    .to_string();

    if let Some(r) = check_cache(paths, cache_name, &version, binary_names, target)? {
        return Ok(Some(r));
    }

    eprintln!(
        "soldr: manifest-first hit for {}/{} {} → {}",
        repo.owner, repo.repo, matched_entry.tag, matched_entry.asset
    );

    // soldr#1790: time the manifest-first (published catalogue) asset
    // download.
    let download_started_at_ms = current_unix_ms();
    let download_started = std::time::Instant::now();
    let downloaded = manifest_lookup::materialize_catalogue_entry(paths, matched_entry).await?;
    let binary_path = archive::extract_catalogue_asset_with_pin(
        paths,
        cache_name,
        &version,
        matched_entry,
        downloaded.path(),
        target,
        binary_names,
    )
    .await?;
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
        version,
        cached: false,
    }))
}

/// soldr#1790: current time as unix milliseconds, for timing recorded
/// fetches. soldr-core has no general-purpose "now in unix ms" helper
/// (only the build-log timestamp *formatter*), so this is computed
/// inline; clock-before-epoch is clamped to 0 rather than panicking.
fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn annotate_release_fetch_error(
    err: SoldrError,
    repo: &github::RepoInfo,
    version: &VersionSpec,
    target: &TargetTriple,
) -> SoldrError {
    let version_desc = match version {
        VersionSpec::Latest => "latest".to_string(),
        VersionSpec::Exact(value) => value.clone(),
    };
    let prefix = format!(
        "release lookup failed for {}/{} requested version {} target {}",
        repo.owner,
        repo.repo,
        version_desc,
        target.triple()
    );
    match err {
        SoldrError::ToolNotFound(message) => {
            SoldrError::ToolNotFound(format!("{prefix}: {message}"))
        }
        SoldrError::Network(message) => SoldrError::Network(format!("{prefix}: {message}")),
        SoldrError::UnsupportedPlatform(message) => {
            SoldrError::UnsupportedPlatform(format!("{prefix}: {message}"))
        }
        SoldrError::Other(message) => SoldrError::Other(format!("{prefix}: {message}")),
        other => SoldrError::Other(format!("{prefix}: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Local cache
// ---------------------------------------------------------------------------

pub(super) fn check_cache(
    paths: &SoldrPaths,
    cache_name: &str,
    version: &str,
    binary_names: &[&str],
    target: &TargetTriple,
) -> Result<Option<FetchResult>, SoldrError> {
    let tool_dir = paths.bin.join(format!("{cache_name}-{version}"));
    let bin_name = format!(
        "{}{}",
        binary_names
            .first()
            .ok_or_else(|| SoldrError::Other(format!(
                "no binary names configured for {cache_name}"
            )))?,
        target.binary_ext()
    );
    let binary_path = tool_dir.join(&bin_name);

    if binary_names.iter().all(|binary_name| {
        tool_dir
            .join(format!("{binary_name}{}", target.binary_ext()))
            .exists()
    }) {
        Ok(Some(FetchResult {
            binary_path,
            version: version.to_string(),
            cached: true,
        }))
    } else {
        Ok(None)
    }
}
