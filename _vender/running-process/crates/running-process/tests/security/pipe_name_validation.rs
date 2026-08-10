use running_process::broker::lifecycle::names::{
    explicit_instance_pipe, private_broker_pipe, shared_broker_pipe,
};

const VALID_SID_HASH: &str = "0123456789abcdef";

#[test]
fn pipe_builders_reject_malicious_service_segments() {
    for value in [
        "../zccache",
        r"..\zccache",
        "zccache/service",
        "zccache;rm",
        "zccache&&calc",
        "Zccache",
    ] {
        assert!(
            private_broker_pipe(VALID_SID_HASH, value).is_err(),
            "private broker service segment {value:?} must be rejected"
        );
        assert!(
            explicit_instance_pipe(VALID_SID_HASH, value).is_err(),
            "explicit broker instance {value:?} must be rejected"
        );
    }
}

#[test]
fn pipe_builders_reject_invalid_sid_hashes() {
    for sid_hash in [
        "",
        "short",
        "0123456789abcdeg",
        "0123456789ABCDEF",
        "../../../etc/passwd",
    ] {
        assert!(
            shared_broker_pipe(sid_hash).is_err(),
            "SID hash {sid_hash:?} must be rejected"
        );
    }
}
