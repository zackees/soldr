#![no_main]

use libfuzzer_sys::fuzz_target;
use running_process::broker::backend_lifecycle::probe::{
    decode_response_identity, PROBE_NONCE_BYTES,
};

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    let nonce = [0x5A_u8; PROBE_NONCE_BYTES];

    // Raw payload: exercises the short-payload and nonce-mismatch rejections.
    let _ = decode_response_identity(data, &nonce);

    // Matched nonce echo: reaches the untrusted DaemonProcess decode plus the
    // try_from normalization (the squat-detection path).
    let mut payload = Vec::with_capacity(PROBE_NONCE_BYTES + data.len());
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(data);
    if let Ok(identity) = decode_response_identity(&payload, &nonce) {
        // Accepted identities were normalized: the endpoint was present and
        // the digest was exactly 32 bytes, so the proto round-trip holds.
        let proto = identity.to_proto();
        assert!(
            proto.ipc_endpoint.is_some(),
            "accepted probe identity must carry an endpoint"
        );
        assert_eq!(
            proto.exe_sha256.len(),
            32,
            "accepted probe identity must carry a 32-byte digest"
        );
    }
});
