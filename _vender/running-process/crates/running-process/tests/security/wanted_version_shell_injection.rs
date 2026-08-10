use running_process::broker::lifecycle::names::validate_version;

#[test]
fn wanted_version_rejects_shell_metacharacters() {
    for value in [
        "1.0.0; rm -rf /",
        "1.0.0 && calc.exe",
        "1.0.0 | whoami",
        "$(touch pwned)",
        "`calc`",
        "1.0.0 alpha",
        "1.0.0-alpha+build",
    ] {
        assert!(
            validate_version(value).is_err(),
            "wanted_version {value:?} must reject shell metacharacters"
        );
    }
}
