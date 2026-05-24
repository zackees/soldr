//! Unit tests for [`crate::trampoline_workspace`]. Integration tests
//! that spawn `soldr cargo build/check/clippy` against a real cargo live
//! at `crates/soldr-cli/tests/cli_cargo_trampoline_workspace.rs`.

use super::*;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn detect_workspace_verb_recognizes_build() {
    assert_eq!(
        detect_workspace_verb(&argv(&["build"])),
        Some(WorkspaceVerb::Build)
    );
    assert_eq!(
        detect_workspace_verb(&argv(&["b", "--release"])),
        Some(WorkspaceVerb::Build)
    );
}

#[test]
fn detect_workspace_verb_recognizes_check_and_clippy() {
    assert_eq!(
        detect_workspace_verb(&argv(&["check"])),
        Some(WorkspaceVerb::Check)
    );
    assert_eq!(
        detect_workspace_verb(&argv(&["c"])),
        Some(WorkspaceVerb::Check)
    );
    assert_eq!(
        detect_workspace_verb(&argv(&["clippy"])),
        Some(WorkspaceVerb::Clippy)
    );
}

#[test]
fn detect_workspace_verb_ignores_run_and_test() {
    assert_eq!(detect_workspace_verb(&argv(&["run"])), None);
    assert_eq!(detect_workspace_verb(&argv(&["test"])), None);
    assert_eq!(detect_workspace_verb(&argv(&["fmt"])), None);
}

#[test]
fn detect_workspace_verb_skips_global_flags() {
    assert_eq!(
        detect_workspace_verb(&argv(&["--manifest-path", "x.toml", "build"])),
        Some(WorkspaceVerb::Build)
    );
    assert_eq!(
        detect_workspace_verb(&argv(&["+nightly", "check"])),
        Some(WorkspaceVerb::Check)
    );
}

#[test]
fn workspace_sidecar_roundtrips_through_toml() {
    let original = WorkspaceSidecar {
        schema_version: WORKSPACE_SIDECAR_SCHEMA_VERSION,
        verb: "build".to_string(),
        cargo_args_fingerprint: "blake3:cafef00d".to_string(),
        outputs: vec![WorkspaceOutput {
            path: "target/debug/foo".to_string(),
            mtime_nanos: 1,
            size_bytes: 2,
        }],
        source_files: vec![SidecarSource {
            path: "src/main.rs".to_string(),
            mtime_nanos: 3,
            size_bytes: 4,
            content_hash: String::new(),
        }],
        clippy_capture: None,
    };
    let text = toml::to_string(&original).expect("serialize");
    let round: WorkspaceSidecar = toml::from_str(&text).expect("deserialize");
    assert_eq!(original, round);
}

#[test]
fn workspace_sidecar_with_clippy_capture_roundtrips() {
    let original = WorkspaceSidecar {
        schema_version: WORKSPACE_SIDECAR_SCHEMA_VERSION,
        verb: "clippy".to_string(),
        cargo_args_fingerprint: "blake3:beef".to_string(),
        outputs: vec![],
        source_files: vec![SidecarSource {
            path: "src/lib.rs".to_string(),
            mtime_nanos: 10,
            size_bytes: 20,
            content_hash: String::new(),
        }],
        clippy_capture: Some(ClippyCaptureEntry {
            exit_code: 0,
            stdout_path: "workspace-clippy.stdout.gz".to_string(),
            stderr_path: "workspace-clippy.stderr.gz".to_string(),
        }),
    };
    let text = toml::to_string(&original).expect("serialize");
    let round: WorkspaceSidecar = toml::from_str(&text).expect("deserialize");
    assert_eq!(original, round);
}

#[test]
fn gzip_round_trip_preserves_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("capture.gz");
    let original = b"warning: foo\n  --> src/main.rs:1:1\n  |\n  = bar\n".repeat(50);
    write_gzip_file(&path, &original).expect("write");
    let decoded = read_gzip_file(&path).expect("read");
    assert_eq!(decoded, original);
}

#[test]
fn parse_all_stanzas_handles_multiple_outputs() {
    let text = "\
/tmp/target/debug/foo: /tmp/src/main.rs
/tmp/target/debug/libbar.rlib: /tmp/src/lib.rs /tmp/src/util.rs
/tmp/target/debug/libbar.rmeta: /tmp/src/lib.rs /tmp/src/util.rs
";
    let stanzas = parse_all_stanzas(text);
    assert_eq!(stanzas.len(), 3);
    assert_eq!(stanzas[0].output, "/tmp/target/debug/foo");
    assert_eq!(stanzas[1].output, "/tmp/target/debug/libbar.rlib");
    assert_eq!(stanzas[2].output, "/tmp/target/debug/libbar.rmeta");
    assert_eq!(
        stanzas[1].sources,
        vec![
            "/tmp/src/lib.rs".to_string(),
            "/tmp/src/util.rs".to_string()
        ]
    );
}

#[test]
fn parse_all_stanzas_skips_blanks_and_comments() {
    let text = "\n# comment\n\n/tmp/foo: /tmp/bar.rs\n\n";
    let stanzas = parse_all_stanzas(text);
    assert_eq!(stanzas.len(), 1);
    assert_eq!(stanzas[0].output, "/tmp/foo");
}

#[test]
fn workspace_filename_includes_verb() {
    assert_eq!(
        WorkspaceVerb::Build.sidecar_filename(),
        "workspace-build.toml"
    );
    assert_eq!(
        WorkspaceVerb::Check.sidecar_filename(),
        "workspace-check.toml"
    );
    assert_eq!(
        WorkspaceVerb::Clippy.sidecar_filename(),
        "workspace-clippy.toml"
    );
}
