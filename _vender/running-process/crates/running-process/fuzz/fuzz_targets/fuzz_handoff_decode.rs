#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::backend_lib::wire::read_handoff_offer;
use running_process::broker::protocol::{Frame, FrameKind, HandoffAck, HandoffOffer};
use running_process::broker::server::handoff::wire::validate_handoff_frame;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    // Backend side: one framed broker->backend HandoffOffer read (framing +
    // Frame decode + envelope validation + payload decode).
    let mut cursor = Cursor::new(data);
    if let Ok(offer) = read_handoff_offer(&mut cursor) {
        assert!(
            offer.token.len() <= common::MAX_PROTO_INPUT_BYTES,
            "accepted HandoffOffer token exceeded the v1 frame cap"
        );
    }

    // Bare payload decodes for both directions.
    let _ = HandoffOffer::decode(data);
    let _ = HandoffAck::decode(data);

    // Broker side: the validation performed on an untrusted backend ACK
    // before its payload is decoded.
    if let Ok(frame) = Frame::decode(data) {
        if validate_handoff_frame(&frame, FrameKind::Response).is_ok() {
            let _ = HandoffAck::decode(frame.payload.as_slice());
        }
    }
});
