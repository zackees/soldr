//! Self-conformance: prove the #415 `test_support::conformance` kit
//! covers a real consumer integration in ~30 LOC of test body using
//! ONLY the public kit surface.

#![cfg(all(feature = "client", feature = "test-support"))]

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification};
use running_process::broker::protocol::{encode_framed, Frame};
use running_process::test_support::conformance::{
    assert_framed_bytes_decode_to, assert_framed_frame_matches_golden, probe_responds_correctly,
    MixedWireExpect, MixedWireScenario, MixedWireStep,
};

use crate::backend_handle_common;

running_process::register_payload_protocol! {
    /// Private-use lane for the self-conformance test daemon.
    const KIT_PAYLOAD_PROTOCOL: u32 = 0xF415;
}

#[test]
fn conformance_kit_covers_consumer_in_thirty_lines() {
    // --- consumer setup (mirrors what zccache/soldr/fbuild/clud do) ---
    let endpoint = backend_handle_common::test_endpoint();
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30)).unwrap();
    let server = backend_handle_common::spawn_endpoint_probe_once(daemon.clone());
    let mux =
        BackendEndpointMux::new(
            daemon.clone(),
            &[KIT_PAYLOAD_PROTOCOL],
            |buf: &[u8]| match buf.first() {
                None => LegacyClassification::NeedMoreBytes,
                Some(b'L') => LegacyClassification::Legacy,
                Some(_) => LegacyClassification::NotLegacy,
            },
        );
    let frame = Frame::request(KIT_PAYLOAD_PROTOCOL, b"ping".to_vec()).with_request_id(1);
    let frame_wire = encode_framed(&frame).unwrap();
    // --- 1. golden bytes (encode + decode) ---
    assert_framed_frame_matches_golden(&frame, &frame_wire).unwrap();
    assert_framed_bytes_decode_to(&frame_wire, &frame).unwrap();
    // --- 2. live BackendHandle probe ---
    probe_responds_correctly("kit", "1.0.0", &endpoint, &daemon).unwrap();
    server.join().unwrap().unwrap();
    // --- 3. mixed-wire harness: legacy 0x01 collision, probe, payload, errors ---
    MixedWireScenario::new()
        .step(MixedWireStep {
            bytes: b"Lhello".to_vec(),
            expect: MixedWireExpect::Legacy,
        })
        .step(MixedWireStep {
            bytes: frame_wire,
            expect: MixedWireExpect::Payload {
                payload_protocol: KIT_PAYLOAD_PROTOCOL,
            },
        })
        .step(MixedWireStep {
            bytes: encode_framed(&Frame::request(0x7002, vec![])).unwrap(),
            expect: MixedWireExpect::Error {
                error_contains: "Unserved".into(),
            },
        })
        .run(&mux)
        .unwrap();
}
