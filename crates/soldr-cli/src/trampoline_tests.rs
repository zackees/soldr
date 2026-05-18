//! Unit tests for [`crate::trampoline`]: the cargo-run arg parser, the
//! sidecar (de)serializer, layout resolution, and the dep-info parser.
//! Lives in a sibling file referenced via `#[path]` so `trampoline.rs`
//! stays comfortably under the 1000-LOC ceiling (post-#339 convention).

use super::*;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn strips_no_trampoline_flag() {
    let (cleaned, saw) = strip_no_trampoline_flag(&argv(&[
        "run",
        "--no-trampoline",
        "--release",
        "--",
        "--no-trampoline",
    ]));
    assert!(saw);
    assert_eq!(
        cleaned,
        argv(&["run", "--release", "--", "--no-trampoline"])
    );
}

#[test]
fn parses_basic_run() {
    let parsed = parse_run_args(&argv(&["run", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
    assert!(!parsed.release);
    assert!(parsed.trailing.is_empty());
}

#[test]
fn parses_release_and_features() {
    let parsed = parse_run_args(&argv(&[
        "run",
        "--release",
        "--bin=demo",
        "--features",
        "a,b",
        "-F",
        "c",
        "--",
        "arg1",
        "arg2",
    ]))
    .expect("parsed");
    assert!(parsed.release);
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
    assert_eq!(parsed.features, vec!["a", "b", "c"]);
    assert_eq!(
        parsed.trailing,
        vec!["arg1".to_string(), "arg2".to_string()]
    );
}

#[test]
fn parses_toolchain_prefix() {
    let parsed = parse_run_args(&argv(&["+nightly", "run", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.toolchain.as_deref(), Some("nightly"));
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
}

#[test]
fn example_falls_through() {
    assert!(parse_run_args(&argv(&["run", "--example", "demo"])).is_none());
}

#[test]
fn unknown_flag_falls_through() {
    assert!(parse_run_args(&argv(&["run", "--frobnicate"])).is_none());
}

#[test]
fn non_run_subcommand_falls_through() {
    assert!(parse_run_args(&argv(&["build"])).is_none());
    assert!(parse_run_args(&argv(&["test"])).is_none());
}

#[test]
fn run_short_alias_is_recognized() {
    let parsed = parse_run_args(&argv(&["r", "--bin", "demo"])).expect("parsed");
    assert_eq!(parsed.bin.as_deref(), Some("demo"));
}

#[test]
fn split_features_handles_commas_and_spaces() {
    assert_eq!(split_features("a,b c"), vec!["a", "b", "c"]);
    assert_eq!(split_features("  "), Vec::<String>::new());
}

#[test]
fn dep_info_parses_simple_stanza() {
    let text = "/tmp/target/debug/foo: /tmp/src/main.rs /tmp/src/lib.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.contains(&"/tmp/src/main.rs".to_string()));
    assert!(strs.contains(&"/tmp/src/lib.rs".to_string()));
}

#[test]
fn dep_info_picks_correct_stanza_among_many() {
    let text = "\
/tmp/target/debug/foo.rlib: /tmp/src/lib.rs
/tmp/target/debug/foo: /tmp/src/main.rs
";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert_eq!(strs, vec!["/tmp/src/main.rs".to_string()]);
}

#[test]
fn dep_info_honors_escaped_spaces() {
    let text = "/tmp/target/debug/foo: /tmp/path\\ with\\ spaces/main.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert_eq!(strs, vec!["/tmp/path with spaces/main.rs".to_string()]);
}

#[test]
fn dep_info_joins_line_continuations() {
    let text = "/tmp/target/debug/foo: /tmp/a.rs \\\n /tmp/b.rs \\\n /tmp/c.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("/tmp/target/debug/foo")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.contains(&"/tmp/a.rs".to_string()));
    assert!(strs.contains(&"/tmp/b.rs".to_string()));
    assert!(strs.contains(&"/tmp/c.rs".to_string()));
}

#[cfg(windows)]
#[test]
fn dep_info_handles_windows_drive_letters() {
    let text = "C:\\target\\debug\\foo.exe: C:\\src\\main.rs C:\\src\\lib.rs\n";
    let sources =
        parse_dep_info_for_output(text, Path::new("C:\\target\\debug\\foo.exe")).expect("parsed");
    let strs: Vec<String> = sources
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    assert!(strs.iter().any(|s| s.ends_with("main.rs")));
    assert!(strs.iter().any(|s| s.ends_with("lib.rs")));
}

#[test]
fn sidecar_roundtrips_through_toml() {
    let original = Sidecar {
        binary_path: "target/debug/foo".to_string(),
        binary_mtime_nanos: 12345,
        binary_size_bytes: 9999,
        cargo_args_fingerprint: "blake3:deadbeef".to_string(),
        source_files: vec![SidecarSource {
            path: "src/main.rs".to_string(),
            mtime_nanos: 678,
            size_bytes: 4096,
        }],
    };
    let text = toml::to_string(&original).expect("serialize");
    let round: Sidecar = toml::from_str(&text).expect("deserialize");
    assert_eq!(original, round);
}

#[test]
fn write_sidecar_writes_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sub").join("foo.toml");
    let data = Sidecar {
        binary_path: "x".into(),
        binary_mtime_nanos: 1,
        binary_size_bytes: 2,
        cargo_args_fingerprint: "blake3:0".into(),
        source_files: vec![],
    };
    write_sidecar_atomic(&path, &data).expect("write");
    assert!(path.is_file());
    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("blake3:0"));
    // Ensure the temp file is not lingering.
    let tmp = path.with_file_name("foo.toml.tmp");
    assert!(!tmp.exists());
}

#[test]
fn compute_layout_uses_release_directory() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: true,
        profile: None,
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: None,
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    assert!(layout.binary_path.to_string_lossy().contains("release"));
    assert!(layout
        .sidecar_path
        .to_string_lossy()
        .contains(".soldr-trampoline"));
}

#[test]
fn compute_layout_with_target_triple() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: false,
        profile: None,
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    let bin_str = layout.binary_path.to_string_lossy();
    assert!(bin_str.contains("x86_64-unknown-linux-gnu"));
    assert!(bin_str.contains("debug"));
}

#[test]
fn compute_layout_custom_profile_maps_dev_to_debug() {
    let parsed = ParsedRunArgs {
        toolchain: None,
        bin: Some("foo".to_string()),
        release: false,
        profile: Some("dev".to_string()),
        manifest_path: Some(PathBuf::from("/tmp/proj/Cargo.toml")),
        target: None,
        features: vec![],
        all_features: false,
        no_default_features: false,
        target_dir: None,
        trailing: vec![],
    };
    let layout = compute_layout(&parsed, "foo");
    assert!(layout.binary_path.to_string_lossy().contains("debug"));
}

#[test]
fn trailing_user_args_extracts_after_separator() {
    assert_eq!(
        trailing_user_args(&argv(&["run", "--bin", "demo", "--", "foo", "bar"])),
        vec!["foo".to_string(), "bar".to_string()]
    );
    assert!(trailing_user_args(&argv(&["run", "--bin", "demo"])).is_empty());
}
