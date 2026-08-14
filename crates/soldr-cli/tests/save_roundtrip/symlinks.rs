//! ---------------------------------------------------------------------------
//! #1548 — symlinked Cargo inputs / cache outputs
//!
//! Symlinks are archived as manifest-only `cache_symlinks` entries (no tar
//! symlink entries), with purely-lexical relative-target validation on BOTH
//! sides: absolute / root-escaping / broken links are skipped loudly at save
//! time, and re-validated at load time so a crafted manifest can never make
//! `load` create a link that points outside the restore root.
//!
//! Gated #[cfg(unix)]: creating symlinks on Windows requires either admin or
//! Developer Mode, so CI Windows lanes can't exercise creation reliably. The
//! pure validation logic has cross-platform unit tests inside save.rs.
//! ---------------------------------------------------------------------------
//!
//! Split out of `save_roundtrip.rs` (soldr#2493): converting `timed_test!`
//! call sites to plain `#[test] fn` costs a line per test, which pushed that
//! already-over-ceiling file further over. This module was already a
//! self-contained `mod symlinks { .. }` block, so it is the natural seam.

use super::*;
use prost::Message as _;
use soldr_cli::cache_lib::save::{Manifest, SymlinkEntry, MANIFEST_NAME};

fn symlink(target: impl AsRef<Path>, destination: impl AsRef<Path>) -> std::io::Result<()> {
    soldr_platform::fs::links::create(
        target.as_ref().to_string_lossy().as_ref(),
        destination.as_ref(),
        false,
    )
}

fn read_link_str(path: &Path) -> String {
    fs::read_link(path).unwrap().to_string_lossy().into_owned()
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn save_full(cache: &Path, archive: &Path) -> soldr_cli::cache_lib::save::SaveReport {
    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(cache),
        out: archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok")
}

fn load_into(archive: &Path, cache: &Path) -> soldr_cli::cache_lib::save::LoadReport {
    load(&LoadOptions {
        archive,
        cache_dir: Some(cache),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok")
}

#[test]
fn cache_symlinks_roundtrip_into_fresh_root() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    write(&cache.join("deps/libfoo.rlib"), b"rlib-bytes");
    symlink("libfoo.rlib", cache.join("deps/libfoo-link.rlib")).unwrap();
    fs::create_dir_all(cache.join("out")).unwrap();
    // Relative target that traverses UP but stays inside the root.
    symlink("../deps/libfoo.rlib", cache.join("out/nested-link")).unwrap();

    let archive = dir.path().join("a.tar.zst");
    let sreport = save_full(&cache, &archive);
    assert_eq!(
        sreport.cache_symlinks, 2,
        "both in-root symlinks must be recorded"
    );
    assert_eq!(sreport.cache_symlinks_skipped, 0);
    // Symlinks must not inflate the regular cache file count.
    assert_eq!(sreport.cache_files, 1);

    let manifest = read_manifest_from_archive(&archive).expect("manifest");
    let mut links: Vec<(String, String)> = manifest
        .cache_symlinks
        .iter()
        .map(|e| (e.path.clone(), e.target.clone()))
        .collect();
    links.sort();
    assert_eq!(
        links,
        vec![
            (
                "deps/libfoo-link.rlib".to_string(),
                "libfoo.rlib".to_string()
            ),
            (
                "out/nested-link".to_string(),
                "../deps/libfoo.rlib".to_string()
            ),
        ]
    );

    // Restore into a FRESH root: the links must come back exactly.
    let fresh = dir.path().join("fresh");
    let lreport = load_into(&archive, &fresh);
    assert_eq!(lreport.cache_symlinks_restored, 2);
    assert_eq!(lreport.cache_symlinks_skipped, 0);

    let link = fresh.join("deps/libfoo-link.rlib");
    assert!(is_symlink(&link), "restored path must BE a symlink");
    assert_eq!(read_link_str(&link), "libfoo.rlib");
    assert_eq!(fs::read(&link).unwrap(), b"rlib-bytes");

    let nested = fresh.join("out/nested-link");
    assert!(is_symlink(&nested));
    assert_eq!(read_link_str(&nested), "../deps/libfoo.rlib");
    assert_eq!(fs::read(&nested).unwrap(), b"rlib-bytes");
}

#[test]
fn load_restores_retargeted_symlink_to_archived_target() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    write(&cache.join("a.bin"), b"content-a");
    write(&cache.join("b.bin"), b"content-b");
    symlink("a.bin", cache.join("current")).unwrap();

    let archive = dir.path().join("a.tar.zst");
    let sreport = save_full(&cache, &archive);
    assert_eq!(sreport.cache_symlinks, 1);

    // Retarget the link on disk after the save.
    fs::remove_file(cache.join("current")).unwrap();
    symlink("b.bin", cache.join("current")).unwrap();
    assert_eq!(fs::read(cache.join("current")).unwrap(), b"content-b");

    // Loading the archive back must restore the ARCHIVED target.
    let lreport = load_into(&archive, &cache);
    assert_eq!(lreport.cache_symlinks_restored, 1);
    assert_eq!(read_link_str(&cache.join("current")), "a.bin");
    assert_eq!(fs::read(cache.join("current")).unwrap(), b"content-a");
}

#[test]
fn save_skips_absolute_escaping_and_broken_symlinks_loudly() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, b"external secret").unwrap();
    write(&cache.join("real.bin"), b"payload");

    // Absolute target (even though it exists) — conservative skip.
    symlink(&outside, cache.join("abs-link")).unwrap();
    // Relative target escaping the cache root — conservative skip.
    symlink("../outside.txt", cache.join("escape-link")).unwrap();
    // Broken in-root target — conservative skip (a dangling link is
    // never silently recreated; consumers go Dirty instead).
    symlink("missing.bin", cache.join("broken-link")).unwrap();

    let archive = dir.path().join("a.tar.zst");
    let sreport = save_full(&cache, &archive);
    assert_eq!(sreport.cache_symlinks, 0, "no unsafe link may be archived");
    assert_eq!(sreport.cache_symlinks_skipped, 3);
    assert_eq!(sreport.cache_files, 1, "only real.bin is a cache file");

    let manifest = read_manifest_from_archive(&archive).expect("manifest");
    assert!(manifest.cache_symlinks.is_empty());

    // A fresh restore contains no trace of the skipped links.
    let fresh = dir.path().join("fresh");
    let lreport = load_into(&archive, &fresh);
    assert_eq!(lreport.cache_symlinks_restored, 0);
    for name in ["abs-link", "escape-link", "broken-link"] {
        assert!(
            fs::symlink_metadata(fresh.join(name)).is_err(),
            "{name} must not exist after restore"
        );
    }
    assert_eq!(fs::read(fresh.join("real.bin")).unwrap(), b"payload");
}

#[test]
fn load_refuses_crafted_escaping_symlink_manifest() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    // Adversarial archive: manifest symlink entries that point outside
    // the restore root. `load` must re-validate and refuse them even
    // though save-side validation never produced them.
    let dir = tempfile::tempdir().unwrap();
    let manifest = Manifest {
        version: 1,
        saved_at_ms: 0,
        workspace: String::new(),
        cache_dir_name: "cache".to_string(),
        files: Vec::new(),
        source_file_count: 0,
        cache_file_count: 0,
        cache_layer_kind: CacheLayerKind::Complete as i32,
        cache_files: Vec::new(),
        base_manifest_blake3: Vec::new(),
        deleted_cache_paths: Vec::new(),
        cache_symlinks: vec![
            SymlinkEntry {
                path: "evil-escape".to_string(),
                target: "../../pwned".to_string(),
                is_dir: false,
            },
            SymlinkEntry {
                path: "evil-abs".to_string(),
                target: "/tmp/pwned".to_string(),
                is_dir: false,
            },
            SymlinkEntry {
                path: "ok-link".to_string(),
                target: "payload.bin".to_string(),
                is_dir: false,
            },
        ],
    };
    let mut manifest_bytes = Vec::new();
    manifest.encode(&mut manifest_bytes).unwrap();

    let archive = dir.path().join("crafted.tar.zst");
    {
        let out = fs::File::create(&archive).unwrap();
        let mut enc = zstd::stream::write::Encoder::new(out, 1).unwrap();
        {
            let mut tar = tar::Builder::new(&mut enc);
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest_bytes.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(&mut header, MANIFEST_NAME, &manifest_bytes[..])
                .unwrap();
            let body: &[u8] = b"payload";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(&mut header, "cache/payload.bin", body)
                .unwrap();
            tar.finish().unwrap();
        }
        enc.finish().unwrap();
    }

    // Restore into a root nested two levels below the tempdir so the
    // "../../pwned" escape would land INSIDE the tempdir if traversal
    // were allowed — observable without touching the real filesystem.
    let root = dir.path().join("nest/root");
    let lreport = load_into(&archive, &root);
    assert_eq!(lreport.cache_symlinks_restored, 1, "only ok-link");
    assert_eq!(lreport.cache_symlinks_skipped, 2);

    assert!(is_symlink(&root.join("ok-link")));
    assert_eq!(fs::read(root.join("ok-link")).unwrap(), b"payload");
    assert!(
        fs::symlink_metadata(root.join("evil-escape")).is_err(),
        "escaping link must not be created"
    );
    assert!(
        fs::symlink_metadata(root.join("evil-abs")).is_err(),
        "absolute-target link must not be created"
    );
    assert!(
        fs::symlink_metadata(dir.path().join("pwned")).is_err(),
        "nothing may be written outside the restore root"
    );
}

#[test]
fn workspace_symlinked_source_surfaced_via_target_content() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let outside = dir.path().join("outside.rs");
    fs::write(&outside, b"pub fn external() {}\n").unwrap();
    write(&ws.join("src/real.rs"), b"pub fn real() {}\n");
    // In-workspace link: must be surfaced, hashed via target content.
    symlink("real.rs", ws.join("src/alias.rs")).unwrap();
    // External + broken links: conservatively omitted.
    symlink(&outside, ws.join("src/external.rs")).unwrap();
    symlink("nope.rs", ws.join("src/broken.rs")).unwrap();

    let archive = dir.path().join("m.tar.zst");
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: None,
        out: &archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: true,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    let manifest = read_manifest_from_archive(&archive).expect("manifest");
    let by_path: std::collections::HashMap<&str, &[u8]> = manifest
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.blake3.as_slice()))
        .collect();
    let real_hash = by_path.get("src/real.rs").expect("real.rs in manifest");
    let alias_hash = by_path
        .get("src/alias.rs")
        .expect("in-workspace symlinked source must be surfaced (#1548)");
    assert_eq!(
        real_hash, alias_hash,
        "symlinked source must hash via its target content"
    );
    assert!(
        !by_path.contains_key("src/external.rs"),
        "out-of-workspace link target stays conservatively omitted"
    );
    assert!(
        !by_path.contains_key("src/broken.rs"),
        "broken link stays conservatively omitted"
    );
}

#[test]
fn delta_load_tombstones_removed_symlink() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    write(&cache.join("keep.bin"), b"kept");
    symlink("keep.bin", cache.join("stay-link")).unwrap();
    symlink("keep.bin", cache.join("gone-link")).unwrap();

    let base_archive = dir.path().join("base.tar.zst");
    let sreport = save_full(&cache, &base_archive);
    assert_eq!(sreport.cache_symlinks, 2);
    let base_manifest = read_manifest_from_archive(&base_archive).unwrap();

    // Delete one link, then cut a delta against the base.
    fs::remove_file(cache.join("gone-link")).unwrap();
    let delta_archive = dir.path().join("delta.tar.zst");
    let dreport = save_delta(&SaveDeltaOptions {
        workspace: None,
        cache_dir: &cache,
        base_manifest: &base_manifest,
        out: &delta_archive,
        zstd_level: 1,
        threads: None,
        profile: SaveProfile::Full,
    })
    .expect("save_delta ok");
    assert_eq!(dreport.cache_symlinks, 1, "delta carries surviving link");
    assert!(
        dreport.deleted_cache_files >= 1,
        "removed symlink must tombstone"
    );

    // Fresh root: base restore brings both links back...
    let fresh = dir.path().join("fresh");
    let breport = load_into(&base_archive, &fresh);
    assert_eq!(breport.cache_symlinks_restored, 2);
    assert!(is_symlink(&fresh.join("gone-link")));

    // ...and the delta removes the deleted one, keeps the survivor.
    let lreport = load_into(&delta_archive, &fresh);
    assert_eq!(lreport.cache_symlinks_restored, 1);
    assert!(
        fs::symlink_metadata(fresh.join("gone-link")).is_err(),
        "tombstoned symlink must be removed by the delta load"
    );
    assert!(is_symlink(&fresh.join("stay-link")));
    assert_eq!(fs::read(fresh.join("stay-link")).unwrap(), b"kept");
}
