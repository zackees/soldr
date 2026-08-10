#![no_main]

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::CacheManifest;

mod common;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    if let Ok(manifest) = CacheManifest::decode(data) {
        common::assert_manifest_invariants(&manifest);
    }
});
