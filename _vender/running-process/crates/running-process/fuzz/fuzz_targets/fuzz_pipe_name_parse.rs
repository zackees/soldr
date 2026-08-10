#![no_main]

use libfuzzer_sys::fuzz_target;
use running_process::broker::lifecycle::names::{
    backend_pipe, explicit_instance_pipe, private_broker_pipe, shared_broker_pipe,
};

mod common;

fuzz_target!(|data: &[u8]| {
    let input = common::lossy_input(data);
    let mut pieces = input.split('\0');
    let sid_hash = pieces.next().unwrap_or_default();
    let service = pieces.next().unwrap_or_default();
    let instance = pieces.next().unwrap_or_default();

    if let Ok(path) = shared_broker_pipe(sid_hash) {
        common::assert_pipe_path_shape(&path);
    }
    if let Ok(path) = private_broker_pipe(sid_hash, service) {
        common::assert_pipe_path_shape(&path);
    }
    if let Ok(path) = explicit_instance_pipe(sid_hash, instance) {
        common::assert_pipe_path_shape(&path);
    }

    let mut random = [0u8; 16];
    let prefix_len = random.len().min(data.len());
    random[..prefix_len].copy_from_slice(&data[..prefix_len]);
    if let Ok(path) = backend_pipe(sid_hash, &random) {
        common::assert_pipe_path_shape(&path);
    }
});
