//! Pure converter from schema-v5 flat asset-index to schema-v6 nested.
//!
//! Lives in `build_support/` instead of `src/` because the build script
//! `include!()`s it via `#[path]` (so the build script gets a stand-alone
//! compile, with no cycle through the crate it's building). An
//! integration test in `tests/build_support_v5_to_v6.rs` `#[path]`-mods
//! the same file so the pure conversion logic is exercised without
//! depending on the build script.
//!
//! The function takes a v5 body (the flat-array shape published on the
//! `manifest` branch) and emits the v6 nested shape that the runtime
//! `ManifestV6::lookup` consumes:
//!
//! ```json
//! {
//!   "schema_version": 6,
//!   "tools": {
//!     "<owner>/<repo>": {
//!       "<host-triple>": {
//!         "latest": "<version>",
//!         "<version>": {"href": "...", "sha256": "..."},
//!         ...
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Conversion rules (deliberately conservative — the runtime tolerates
//! more shapes than the converter emits):
//!
//! 1. Skip entries whose asset name doesn't end in a known host triple.
//!    The triple is extracted by suffix-matching against the
//!    [`KNOWN_HOST_TRIPLES`] table. A `vendored/<owner>/<repo>` entry,
//!    or a `deps/mac/manifest.json`-style entry, has no triple in its
//!    asset name and is silently dropped.
//! 2. Normalize the version: strip a leading `v` from the v5 `tag` so
//!    the v6 leaf keys are bare semver (`1.12.9`, not `v1.12.9`). The
//!    runtime lookup expects bare semver — see `manifest_v6::ManifestV6::lookup`.
//! 3. Pick `latest` as the lexicographically-greatest stable version
//!    seen for a given `(owner, repo, triple)` triple. Lexicographic on
//!    dot-separated semver gives the right answer for `1.12.9` vs.
//!    `1.12.8`, and crucially is deterministic (no v5→v6 conversion
//!    should depend on iteration order). Prereleases are dropped by
//!    [`is_stable_version_tag`] before the latest pick.
//! 4. Per-leaf duplicate versions: last write wins. The v5 index can
//!    publish the same `(owner, repo, tag, asset)` more than once if a
//!    release is re-cut; we keep the last one.

use std::collections::BTreeMap;

/// The host triples we recognize in v5 asset names. Order matters —
/// longer triples come first so the suffix match doesn't accidentally
/// claim `apple-darwin` when the asset is actually `apple-darwin-arm`.
///
/// Kept deliberately short: only the triples soldr resolves at runtime.
/// New triples must land here AND in `core::TargetTriple` (which is the
/// runtime detector) — drift between the two means a build serves no
/// hit even though the asset is published.
pub const KNOWN_HOST_TRIPLES: &[&str] = &[
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "i686-pc-windows-msvc",
    "i686-unknown-linux-gnu",
];

/// Extract the host triple from a v5 asset name like
/// `zccache-v1.10.0-x86_64-pc-windows-msvc.zip`. The triple is found by
/// scanning [`KNOWN_HOST_TRIPLES`] for a substring match. Returns the
/// matched triple, or `None` if no triple is present (the entry will be
/// skipped by the converter).
pub fn extract_host_triple(asset_name: &str) -> Option<&'static str> {
    KNOWN_HOST_TRIPLES
        .iter()
        .find(|&&triple| asset_name.contains(triple))
        .copied()
}

/// True when `tag` looks like a stable release tag.
///
/// Mirrors `manifest_v6::is_stable_version_tag` (deliberate duplication
/// — `build.rs` cannot reach into the crate it's building). Any drift
/// is caught by [`tests::stable_filter_matches_runtime`], which compares
/// the two functions head-to-head over a fixed corpus.
pub fn is_stable_version_tag(tag: &str) -> bool {
    let trimmed = tag.trim_start_matches('v');
    let mut parts = trimmed.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    all_digits(major) && all_digits(minor) && all_digits(patch)
}

/// Compare two bare-semver strings (`"1.12.9"`, `"1.12.10"`). Returns
/// the standard `Ordering`. Falls back to lexicographic compare on the
/// raw string when either side isn't numeric — keeps the converter
/// total in the face of unexpected version shapes (which
/// [`is_stable_version_tag`] should have filtered out anyway).
fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse =
        |s: &str| -> Option<Vec<u64>> { s.split('.').map(|p| p.parse::<u64>().ok()).collect() };
    match (parse(a), parse(b)) {
        (Some(av), Some(bv)) => av.cmp(&bv),
        _ => a.cmp(b),
    }
}

/// Convert a v5 asset-index JSON body into a v6 manifest JSON body.
///
/// Returns the serialized v6 JSON. Errors only on malformed v5 input
/// (parse failure); empty entries arrays yield a valid empty v6
/// envelope (`{"schema_version":6,"tools":{}}`).
pub fn convert_v5_to_v6(v5_body: &str) -> Result<String, String> {
    let v5: serde_json::Value =
        serde_json::from_str(v5_body).map_err(|e| format!("v5 parse failed: {e}"))?;

    let entries = v5
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "v5 body missing `entries: []`".to_string())?;

    // BTreeMap so the emitted JSON is deterministic — same input always
    // produces byte-identical output, so the compressed embedded blob
    // changes only when the source-of-truth manifest changes.
    let mut tools: BTreeMap<String, BTreeMap<String, BTreeMap<String, serde_json::Value>>> =
        BTreeMap::new();
    let mut latest_per_leaf: BTreeMap<(String, String), String> = BTreeMap::new();

    for entry in entries {
        let Some(owner) = entry.get("owner").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(repo) = entry.get("repo").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(tag) = entry.get("tag").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(asset) = entry.get("asset").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(url) = entry.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(sha256) = entry.get("sha256").and_then(|v| v.as_str()) else {
            continue;
        };

        // Drop entries without a recognizable host triple (e.g. the
        // `vendored/messense/cargo-zigbuild` SDK rows, the
        // `deps/mac/manifest.json` row).
        let Some(triple) = extract_host_triple(asset) else {
            continue;
        };

        // Bare semver only — the converter is the publish-side filter.
        if !is_stable_version_tag(tag) {
            continue;
        }
        let version = tag.trim_start_matches('v').to_string();

        let tool_key = format!("{owner}/{repo}");
        let leaf = tools
            .entry(tool_key.clone())
            .or_default()
            .entry(triple.to_string())
            .or_default();
        let mut asset_obj = serde_json::Map::new();
        asset_obj.insert(
            "href".to_string(),
            serde_json::Value::String(url.to_string()),
        );
        asset_obj.insert(
            "sha256".to_string(),
            serde_json::Value::String(sha256.to_string()),
        );
        leaf.insert(version.clone(), serde_json::Value::Object(asset_obj));

        // Track the lexicographically-greatest version per leaf for
        // the `latest` pointer.
        let key = (tool_key, triple.to_string());
        let pick_new = match latest_per_leaf.get(&key) {
            Some(cur) => cmp_semver(&version, cur) == std::cmp::Ordering::Greater,
            None => true,
        };
        if pick_new {
            latest_per_leaf.insert(key, version);
        }
    }

    // Stamp `latest` on every leaf.
    for ((tool_key, triple), latest) in &latest_per_leaf {
        if let Some(leaf) = tools.get_mut(tool_key).and_then(|t| t.get_mut(triple)) {
            leaf.insert(
                "latest".to_string(),
                serde_json::Value::String(latest.clone()),
            );
        }
    }

    let mut root = serde_json::Map::new();
    root.insert(
        "schema_version".to_string(),
        serde_json::Value::Number(6u32.into()),
    );
    // Re-serialize the nested BTreeMaps as plain JSON objects. The
    // serialization preserves key order because the values are
    // BTreeMap-backed.
    let tools_value = serde_json::to_value(&tools).map_err(|e| format!("tools encode: {e}"))?;
    root.insert("tools".to_string(), tools_value);
    serde_json::to_string(&serde_json::Value::Object(root)).map_err(|e| format!("root encode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_v5() -> &'static str {
        r#"{
            "schema_version": 5,
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.9",
                    "asset": "zccache-1.12.9-x86_64-pc-windows-msvc.zip",
                    "url": "https://example.com/zccache-1.12.9-x86_64-pc-windows-msvc.zip",
                    "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                },
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.8",
                    "asset": "zccache-1.12.8-x86_64-pc-windows-msvc.zip",
                    "url": "https://example.com/zccache-1.12.8-x86_64-pc-windows-msvc.zip",
                    "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                },
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "v1.12.10",
                    "asset": "zccache-v1.12.10-aarch64-apple-darwin.tar.gz",
                    "url": "https://example.com/zccache-v1.12.10-aarch64-apple-darwin.tar.gz",
                    "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                },
                {
                    "owner": "vendored",
                    "repo": "messense/cargo-zigbuild",
                    "tag": "MacOSX11.3",
                    "asset": "sdk.tar.zstd",
                    "url": "https://example.com/sdk.tar.zstd",
                    "sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                },
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "v1.12.0-rc1",
                    "asset": "zccache-v1.12.0-rc1-x86_64-pc-windows-msvc.zip",
                    "url": "https://example.com/zccache-v1.12.0-rc1-x86_64-pc-windows-msvc.zip",
                    "sha256": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                }
            ]
        }"#
    }

    #[test]
    fn converts_basic_v5_to_v6() {
        let v6 = convert_v5_to_v6(sample_v5()).expect("convert ok");
        let parsed: serde_json::Value = serde_json::from_str(&v6).expect("v6 parses");
        assert_eq!(parsed["schema_version"], serde_json::json!(6));
        let leaf = &parsed["tools"]["zackees/zccache"]["x86_64-pc-windows-msvc"];
        assert_eq!(leaf["latest"], serde_json::json!("1.12.9"));
        assert!(leaf.get("1.12.9").is_some());
        assert!(leaf.get("1.12.8").is_some());
    }

    #[test]
    fn skips_entries_with_no_recognized_triple() {
        let v6 = convert_v5_to_v6(sample_v5()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&v6).unwrap();
        // The `vendored/messense/cargo-zigbuild` row had asset
        // `sdk.tar.zstd` — no triple, so the entire tool key must be
        // absent from the v6 output.
        assert!(
            parsed["tools"]
                .get("vendored/messense/cargo-zigbuild")
                .is_none(),
            "vendored entries without a host triple must be skipped"
        );
    }

    #[test]
    fn strips_leading_v_from_versions() {
        let v6 = convert_v5_to_v6(sample_v5()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&v6).unwrap();
        let leaf = &parsed["tools"]["zackees/zccache"]["aarch64-apple-darwin"];
        // tag was `v1.12.10`; v6 key must be the bare `1.12.10`.
        assert!(
            leaf.get("1.12.10").is_some(),
            "leading `v` must be stripped"
        );
        assert!(
            leaf.get("v1.12.10").is_none(),
            "prefixed form must not leak through"
        );
        assert_eq!(leaf["latest"], serde_json::json!("1.12.10"));
    }

    #[test]
    fn latest_picks_highest_stable_per_leaf() {
        let v6 = convert_v5_to_v6(sample_v5()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&v6).unwrap();
        let leaf = &parsed["tools"]["zackees/zccache"]["x86_64-pc-windows-msvc"];
        // We saw 1.12.9, 1.12.8, and a prerelease (v1.12.0-rc1 which
        // is filtered) — latest must be 1.12.9.
        assert_eq!(leaf["latest"], serde_json::json!("1.12.9"));
    }

    #[test]
    fn drops_prerelease_versions() {
        let v6 = convert_v5_to_v6(sample_v5()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&v6).unwrap();
        let leaf = &parsed["tools"]["zackees/zccache"]["x86_64-pc-windows-msvc"];
        // The v1.12.0-rc1 entry must not have produced a leaf entry.
        assert!(
            leaf.get("1.12.0-rc1").is_none(),
            "prerelease versions must be dropped"
        );
        assert!(
            leaf.get("1.12.0").is_none(),
            "prerelease versions must not be silently re-tagged as stable"
        );
    }

    #[test]
    fn empty_v5_yields_empty_v6() {
        let v6 = convert_v5_to_v6(r#"{"schema_version": 5, "entries": []}"#).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&v6).unwrap();
        assert_eq!(parsed["schema_version"], serde_json::json!(6));
        assert_eq!(parsed["tools"], serde_json::json!({}));
    }

    #[test]
    fn malformed_v5_returns_err() {
        assert!(convert_v5_to_v6("not-json").is_err());
        assert!(convert_v5_to_v6(r#"{"schema_version":5}"#).is_err());
    }

    #[test]
    fn extracts_known_triples_from_asset_names() {
        assert_eq!(
            extract_host_triple("zccache-1.12.9-x86_64-pc-windows-msvc.zip"),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            extract_host_triple("cargo-chef-aarch64-apple-darwin.tar.gz"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(extract_host_triple("sdk.tar.zstd"), None);
        assert_eq!(extract_host_triple("manifest.json"), None);
    }

    #[test]
    fn output_is_deterministic() {
        let a = convert_v5_to_v6(sample_v5()).unwrap();
        let b = convert_v5_to_v6(sample_v5()).unwrap();
        assert_eq!(a, b, "conversion must be byte-deterministic");
    }
}
