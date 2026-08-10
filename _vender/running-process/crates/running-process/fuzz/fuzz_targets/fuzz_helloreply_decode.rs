#![no_main]

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::HelloReply;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    let _ = HelloReply::decode(data);
});
