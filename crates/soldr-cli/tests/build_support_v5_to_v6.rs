//! Integration test for the build-script's pure v5→v6 conversion
//! helper. The test #[path]-mods the same source file that `build.rs`
//! consumes, so a single source of truth is exercised by both the build
//! step (offline-tolerant fetch + write) and the unit test (synthetic
//! corpus, no I/O).
//!
//! The conversion logic is intentionally a pure function — it takes a
//! v5 JSON body and returns a v6 JSON body. No filesystem, no network.
//! That's what lets us assert against a small in-memory corpus here
//! without standing up a build-script integration harness.

#[path = "../build_support/v5_to_v6.rs"]
mod v5_to_v6;

use soldr_cli::timed_test;

timed_test!(build_script_v5_to_v6_conversion, {
    let v5 = r#"{
        "schema_version": 5,
        "entries": [
            {
                "owner": "zackees",
                "repo": "zccache",
                "tag": "v1.12.9",
                "asset": "zccache-v1.12.9-x86_64-pc-windows-msvc.zip",
                "url": "https://example.com/zccache-v1.12.9-x86_64-pc-windows-msvc.zip",
                "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
            },
            {
                "owner": "zackees",
                "repo": "zccache",
                "tag": "v1.12.8",
                "asset": "zccache-v1.12.8-x86_64-pc-windows-msvc.zip",
                "url": "https://example.com/zccache-v1.12.8-x86_64-pc-windows-msvc.zip",
                "sha256": "2222222222222222222222222222222222222222222222222222222222222222"
            },
            {
                "owner": "zackees",
                "repo": "zccache",
                "tag": "v1.12.0-rc1",
                "asset": "zccache-v1.12.0-rc1-x86_64-pc-windows-msvc.zip",
                "url": "https://example.com/zccache-v1.12.0-rc1-x86_64-pc-windows-msvc.zip",
                "sha256": "3333333333333333333333333333333333333333333333333333333333333333"
            },
            {
                "owner": "vendored",
                "repo": "messense/cargo-zigbuild",
                "tag": "MacOSX11.3",
                "asset": "sdk.tar.zstd",
                "url": "https://example.com/sdk.tar.zstd",
                "sha256": "4444444444444444444444444444444444444444444444444444444444444444"
            }
        ]
    }"#;
    let v6 = v5_to_v6::convert_v5_to_v6(v5).expect("convert ok");
    let parsed: serde_json::Value = serde_json::from_str(&v6).expect("v6 parses");
    assert_eq!(parsed["schema_version"], serde_json::json!(6));
    let leaf = &parsed["tools"]["zackees/zccache"]["x86_64-pc-windows-msvc"];
    // The two stable releases land as leaf entries.
    assert!(leaf.get("1.12.9").is_some(), "v1.12.9 should be present");
    assert!(leaf.get("1.12.8").is_some(), "v1.12.8 should be present");
    // The rc1 prerelease and the no-triple vendored row must be filtered.
    assert!(leaf.get("1.12.0-rc1").is_none());
    assert!(leaf.get("1.12.0").is_none());
    assert!(parsed["tools"]
        .get("vendored/messense/cargo-zigbuild")
        .is_none());
    // The latest pointer must pick the highest stable version.
    assert_eq!(leaf["latest"], serde_json::json!("1.12.9"));
    // The conversion must be deterministic — re-running over the same
    // input yields byte-identical output.
    let v6_again = v5_to_v6::convert_v5_to_v6(v5).expect("convert ok");
    assert_eq!(
        v6, v6_again,
        "conversion must be deterministic so the embedded blob doesn't churn"
    );
});

timed_test!(v5_triple_extraction_round_trip, {
    assert_eq!(
        v5_to_v6::extract_host_triple("zccache-x86_64-pc-windows-msvc.zip"),
        Some("x86_64-pc-windows-msvc")
    );
    assert_eq!(
        v5_to_v6::extract_host_triple("foo-aarch64-apple-darwin.tar.gz"),
        Some("aarch64-apple-darwin")
    );
    assert_eq!(v5_to_v6::extract_host_triple("sdk.tar.zstd"), None);
});

timed_test!(v5_stable_filter_matches_runtime, {
    // Spot-check that the build-side stable filter agrees with the
    // runtime-side filter on a fixed corpus. Drift between the two
    // means publish-side dropped releases that the runtime would
    // happily resolve, or vice versa.
    let corpus = [
        ("1.0.0", true),
        ("v1.0.0", true),
        ("v1.12.9", true),
        ("0.1.73", true),
        ("v1.0.0-rc1", false),
        ("1.0.0-alpha", false),
        ("1.0.0.dev1", false),
        ("1.0.0+build7", false),
        ("", false),
        ("v1.0", false),
    ];
    for (tag, expected) in corpus {
        let build_side = v5_to_v6::is_stable_version_tag(tag);
        let runtime_side = soldr_cli::fetch::is_stable_version_tag(tag);
        assert_eq!(
            build_side, expected,
            "build-side disagrees with corpus on {tag:?}"
        );
        assert_eq!(
            runtime_side, expected,
            "runtime-side disagrees with corpus on {tag:?}"
        );
        assert_eq!(
            build_side, runtime_side,
            "build-side and runtime-side stable-tag filters disagree on {tag:?}"
        );
    }
});
