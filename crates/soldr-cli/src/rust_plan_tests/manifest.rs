//! Tests for the thin-slice manifest writer / file enumeration in
//! [`crate::rust_plan::build_thin_manifest`] and
//! [`crate::rust_plan::write_thin_manifest`].

use crate::rust_plan::{build_thin_manifest, write_thin_manifest, ThinSliceManifest};
use crate::THIN_MANIFEST_FILENAME;

/// The manifest must enumerate every regular file in the bundle, with
/// relative POSIX-style paths and either a size or `null`. It must NOT
/// list directories or its own filename.
#[test]
fn thin_manifest_enumerates_only_files_actually_present() {
    let bundle = tempfile::tempdir().expect("tempdir for bundle");
    let bundle_path = bundle.path();

    // Build a representative bundle layout: nested dir + a file at root +
    // an empty subdir (must not appear in the manifest).
    std::fs::create_dir_all(bundle_path.join("debug/.fingerprint/foo-abc")).unwrap();
    std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
    std::fs::create_dir_all(bundle_path.join("debug/empty_subdir")).unwrap();
    std::fs::write(
        bundle_path.join("debug/.fingerprint/foo-abc/invoked.timestamp"),
        "",
    )
    .unwrap();
    std::fs::write(
        bundle_path.join("debug/.fingerprint/foo-abc/dep-lib-foo"),
        b"abc123",
    )
    .unwrap();
    std::fs::write(bundle_path.join("debug/deps/foo-abc.d"), b"foo.rs:\n").unwrap();
    std::fs::write(bundle_path.join("CACHEDIR.TAG"), b"Signature: 8a4773\n").unwrap();

    let manifest = build_thin_manifest(bundle_path, "thin-v2").expect("build manifest");

    assert_eq!(manifest.schema_version, 2);
    assert_eq!(manifest.cache_profile, "thin-v2");

    let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    // Sorted, POSIX-style, no manifest self-reference, no empty dir.
    assert_eq!(
        paths,
        vec![
            "CACHEDIR.TAG",
            "debug/.fingerprint/foo-abc/dep-lib-foo",
            "debug/.fingerprint/foo-abc/invoked.timestamp",
            "debug/deps/foo-abc.d",
        ],
    );
    // Sizes are populated for files that exist on disk.
    let by_path: std::collections::HashMap<_, _> = manifest
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.size_bytes))
        .collect();
    assert_eq!(
        by_path.get("debug/.fingerprint/foo-abc/dep-lib-foo"),
        Some(&Some(6))
    );
    assert_eq!(
        by_path.get("debug/.fingerprint/foo-abc/invoked.timestamp"),
        Some(&Some(0))
    );
}

/// The on-disk manifest emitted by `write_thin_manifest` must round-trip
/// through serde so downstream verifiers can deserialize it without
/// surprises (no field renames, no missing fields).
#[test]
fn thin_manifest_round_trips_through_serde() {
    let bundle = tempfile::tempdir().expect("tempdir for manifest round-trip");
    let bundle_path = bundle.path();
    std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
    std::fs::write(
        bundle_path.join("debug/deps/example.d"),
        b"example: src/lib.rs\n",
    )
    .unwrap();

    write_thin_manifest(bundle_path, Some("thin-v2")).expect("write manifest");

    let manifest_path = bundle_path.join(THIN_MANIFEST_FILENAME);
    assert!(
        manifest_path.is_file(),
        "manifest must land at the well-known path"
    );

    let raw = std::fs::read_to_string(&manifest_path).expect("read manifest");
    let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("deserialize manifest");

    assert_eq!(parsed.schema_version, 2);
    assert_eq!(parsed.cache_profile, "thin-v2");
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files[0].path, "debug/deps/example.d");

    // Serializing the parsed value back must produce a JSON document that
    // deserializes to an equal value (canonical round-trip).
    let serialized = serde_json::to_string(&parsed).expect("serialize manifest");
    let reparsed: ThinSliceManifest =
        serde_json::from_str(&serialized).expect("re-deserialize manifest");
    assert_eq!(parsed, reparsed);
}

/// A second `write_thin_manifest` call into the same bundle directory
/// must not list the previously-written manifest among its own entries.
#[test]
fn thin_manifest_does_not_self_reference_on_repeat_save() {
    let bundle = tempfile::tempdir().expect("tempdir for repeat save");
    let bundle_path = bundle.path();
    std::fs::write(bundle_path.join("only.txt"), b"hello").unwrap();

    write_thin_manifest(bundle_path, Some("thin-v2")).expect("first manifest write");
    write_thin_manifest(bundle_path, Some("thin-v2")).expect("second manifest write");

    let raw =
        std::fs::read_to_string(bundle_path.join(THIN_MANIFEST_FILENAME)).expect("read manifest");
    let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("parse manifest");

    let paths: Vec<&str> = parsed.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["only.txt"]);
}
