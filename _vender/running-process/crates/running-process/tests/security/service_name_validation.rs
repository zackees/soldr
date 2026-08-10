use running_process::broker::lifecycle::names::validate_service_name;

#[test]
fn service_name_rejects_path_and_shell_input() {
    for value in [
        "../zccache",
        r"..\zccache",
        "/tmp/zccache",
        "zccache;rm",
        "zccache&&calc",
        "zccache|whoami",
        "zccache service",
        "zccache.service",
        "zccache_service",
        "Zccache",
    ] {
        assert!(
            validate_service_name(value).is_err(),
            "service_name {value:?} must be rejected"
        );
    }
}

#[test]
fn service_name_boundary_is_sixty_four_bytes() {
    validate_service_name(&"a".repeat(64)).expect("64-byte service names are allowed");
    assert!(
        validate_service_name(&"a".repeat(65)).is_err(),
        "65-byte service names must be rejected"
    );
}
