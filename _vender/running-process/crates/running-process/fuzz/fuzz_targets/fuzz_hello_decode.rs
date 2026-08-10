#![no_main]

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::Hello;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    if let Ok(hello) = Hello::decode(data) {
        common::assert_hello_invariants(&hello);
    }
});
