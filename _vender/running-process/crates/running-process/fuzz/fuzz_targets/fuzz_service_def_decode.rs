#![no_main]

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::ServiceDefinition;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    if let Ok(service_def) = ServiceDefinition::decode(data) {
        common::assert_service_definition_invariants(&service_def);
    }
});
