use running_process::broker::lifecycle::names::validate_version;

#[test]
fn wanted_version_rejects_path_traversal() {
    for value in [
        "../../../bin/evil",
        r"..\..\bin\evil",
        "/tmp/evil",
        r"C:\temp\evil.exe",
        "1.0.0/evil",
        "1.0.0-../../evil",
        "1.0.0..evil",
    ] {
        assert!(
            validate_version(value).is_err(),
            "wanted_version {value:?} must reject path traversal"
        );
    }
}

#[test]
fn wanted_version_accepts_canonical_semver() {
    for value in ["0.0.1", "1.2.3", "1.2.3-alpha.1"] {
        validate_version(value).unwrap_or_else(|err| {
            panic!("wanted_version {value:?} should be accepted: {err}");
        });
    }
}
