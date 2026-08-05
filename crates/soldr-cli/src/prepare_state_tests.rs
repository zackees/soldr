//! Save/restore state tests separated from `prepare_cmd` for the LOC ratchet.

use crate::core::SoldrPaths;
use crate::prepare_cmd::{
    blessed_xwin_cache_root, classify_target, expected_state_paths, prepare_state_roots,
    restore_prepare_state, save_prepare_state,
};

crate::timed_test!(expected_state_paths_uses_blessed_msvc_xwin_cache, {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let attrs = classify_target("x86_64-pc-windows-msvc").expect("classify");
    let xwin_root = blessed_xwin_cache_root(&paths, "x86_64-pc-windows-msvc");

    let entries = expected_state_paths(&attrs, &paths).expect("expected paths");
    let xwin_entry = entries
        .iter()
        .find(|entry| entry.label == "xwin MSVC CRT + Windows SDK")
        .expect("xwin restore entry");
    assert_eq!(xwin_entry.path, xwin_root);
    assert!(!xwin_entry.present);

    std::fs::create_dir_all(xwin_root.join("xwin").join("crt").join("include"))
        .expect("mkdir crt include");
    std::fs::create_dir_all(xwin_root.join("xwin").join("sdk").join("include"))
        .expect("mkdir sdk include");
    std::fs::write(xwin_root.join(".complete"), b"").expect("write complete");

    let entries = expected_state_paths(&attrs, &paths).expect("expected paths");
    assert!(entries
        .iter()
        .any(|entry| entry.label == "xwin MSVC CRT + Windows SDK" && entry.present));
});

crate::timed_test!(prepare_state_roots_includes_blessed_sdk_root, {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let sdk_root = paths.root.join("sdk");
    std::fs::create_dir_all(&sdk_root).expect("mkdir sdk root");
    let roots = prepare_state_roots(&paths).expect("prepare roots");
    assert!(roots.iter().any(|root| root == &sdk_root));
});

crate::timed_test!(gnu_restore_state_uses_the_catalogue_bundle_not_zig, {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let paths = SoldrPaths::with_root(tmp.path().join("soldr"));
    let attrs = classify_target("aarch64-unknown-linux-gnu").expect("classify GNU");
    assert!(
        !attrs.needs_zig,
        "GNU must not advertise a Zig restore path"
    );

    let entries = expected_state_paths(&attrs, &paths).expect("expected paths");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].label.contains("GNU/Linux toolchain"));
    assert!(!entries[0].label.to_ascii_lowercase().contains("zig"));

    let root = paths.bin.join("syslib").join("gnu-linux-toolchain");
    std::fs::create_dir_all(&root).expect("mkdir GNU root");
    let roots = prepare_state_roots(&paths).expect("prepare roots");
    assert!(roots.iter().any(|candidate| candidate == &root));
});

crate::timed_test!(prepare_state_archive_restores_to_a_different_soldr_root, {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let source = SoldrPaths::with_root(tmp.path().join("source-soldr"));
    let gnu_marker = source
        .bin
        .join("syslib")
        .join("gnu-linux-toolchain")
        .join("ready");
    let sdk_marker = source.root.join("sdk").join("ready");
    let zig_marker = source.bin.join("zig-0.14.1").join("ready");
    let zlib_marker = source.bin.join("syslib/zlib-ng/ready");
    std::fs::create_dir_all(gnu_marker.parent().expect("GNU parent")).expect("mkdir GNU");
    std::fs::create_dir_all(sdk_marker.parent().expect("SDK parent")).expect("mkdir SDK");
    std::fs::write(&gnu_marker, b"gnu").expect("write GNU marker");
    std::fs::write(&sdk_marker, b"sdk").expect("write SDK marker");
    std::fs::create_dir_all(zig_marker.parent().expect("Zig parent")).expect("mkdir Zig");
    std::fs::write(&zig_marker, b"zig").expect("write Zig marker");
    std::fs::create_dir_all(zlib_marker.parent().expect("zlib parent")).expect("mkdir zlib");
    std::fs::write(&zlib_marker, b"zlib").expect("write zlib marker");

    let archive = tmp.path().join("prepared.tar.zst");
    save_prepare_state(&archive, &source, "x86_64-unknown-linux-gnu").expect("save prepare state");

    let restored = SoldrPaths::with_root(tmp.path().join("restored-soldr"));
    restore_prepare_state(&archive, &restored).expect("restore prepare state");
    assert_eq!(
        std::fs::read(restored.bin.join("syslib/gnu-linux-toolchain/ready")).expect("GNU restored"),
        b"gnu"
    );
    assert_eq!(
        std::fs::read(restored.root.join("sdk/ready")).expect("SDK restored"),
        b"sdk"
    );
    assert_eq!(
        std::fs::read(restored.bin.join("syslib/zlib-ng/ready")).expect("zlib restored"),
        b"zlib"
    );
    assert!(
        !restored.bin.join("zig-0.14.1/ready").exists(),
        "GNU archive must not capture unrelated Zig state"
    );
});
