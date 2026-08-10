#![allow(dead_code)]

use running_process::broker::lifecycle::names::{
    validate_service_name, validate_version, PipePath,
};
use running_process::broker::protocol::{CacheManifest, Frame, Hello, ServiceDefinition};

pub const MAX_SERVICE_NAME_BYTES: usize = 64;
pub const MAX_VERSION_BYTES: usize = 64;
pub const MAX_CLIENT_VERSION_BYTES: usize = 128;
pub const MAX_CLIENT_LIB_BYTES: usize = 64;
pub const FUZZ_FRAME_READ_CAP: usize = running_process::broker::MAX_HELLO_SIZE_BYTES;
pub const MAX_PROTO_INPUT_BYTES: usize = running_process::broker::MAX_FRAME_SIZE_BYTES;

pub fn skip_oversize_proto_input(data: &[u8]) -> bool {
    data.len() > MAX_PROTO_INPUT_BYTES
}

pub fn assert_frame_invariants(frame: &Frame) {
    assert!(
        frame.payload.len() <= MAX_PROTO_INPUT_BYTES,
        "decoded Frame payload exceeded the v1 frame cap"
    );
}

pub fn assert_hello_invariants(hello: &Hello) {
    if validate_service_name(&hello.service_name).is_ok() {
        assert_valid_service_name(&hello.service_name);
    }
    if validate_version(&hello.wanted_version).is_ok() {
        assert_valid_version_shape(&hello.wanted_version);
    }
}

pub fn assert_manifest_invariants(manifest: &CacheManifest) {
    if validate_service_name(&manifest.service_name).is_ok() {
        assert_valid_service_name(&manifest.service_name);
    }
    if validate_version(&manifest.service_version).is_ok() {
        assert_valid_version_shape(&manifest.service_version);
    }
    for dependency in &manifest.depends_on {
        if validate_service_name(&dependency.service_name).is_ok() {
            assert_valid_service_name(&dependency.service_name);
        }
        if validate_version(&dependency.min_version).is_ok() {
            assert_valid_version_shape(&dependency.min_version);
        }
    }
}

pub fn assert_service_definition_invariants(service_def: &ServiceDefinition) {
    if validate_service_name(&service_def.service_name).is_ok() {
        assert_valid_service_name(&service_def.service_name);
    }
    if validate_service_name(&service_def.explicit_instance).is_ok() {
        assert_valid_service_name(&service_def.explicit_instance);
    }
    if validate_version(&service_def.min_version).is_ok() {
        assert_valid_version_shape(&service_def.min_version);
    }
    for version in &service_def.version_allow_list {
        if validate_version(version).is_ok() {
            assert_valid_version_shape(version);
        }
    }
}

pub fn assert_valid_service_name(name: &str) {
    assert!(!name.is_empty(), "accepted service_name must not be empty");
    assert!(
        name.len() <= MAX_SERVICE_NAME_BYTES,
        "accepted service_name exceeded 64 bytes"
    );
    assert!(
        name.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "accepted service_name contained a forbidden byte"
    );
}

pub fn assert_valid_version_shape(version: &str) {
    assert!(
        version
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'),
        "accepted version contained shell/path metacharacters"
    );
}

pub fn assert_pipe_path_shape(path: &PipePath) {
    match (&path.windows, &path.unix) {
        (Some(windows), None) => {
            assert!(windows.starts_with(r"\\.\pipe\rpb-v1-"));
            assert!(windows.len() <= 260);
        }
        (None, Some(unix)) => {
            let rendered = unix.to_string_lossy();
            assert!(rendered.ends_with(".sock"));
            #[cfg(target_os = "macos")]
            assert!(rendered.len() < 104);
            #[cfg(all(unix, not(target_os = "macos")))]
            assert!(rendered.len() < 108);
        }
        _ => panic!("PipePath must populate exactly one platform path"),
    }
}

pub fn lossy_input(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}
