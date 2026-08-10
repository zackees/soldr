#![no_main]

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::Frame;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    if let Ok(frame) = Frame::decode(data) {
        common::assert_frame_invariants(&frame);
    }
});
