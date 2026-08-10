#![no_main]

use libfuzzer_sys::fuzz_target;
use running_process::broker::lifecycle::names::validate_service_name;

mod common;

fuzz_target!(|data: &[u8]| {
    let raw = common::lossy_input(data);

    if validate_service_name(&raw).is_ok() {
        common::assert_valid_service_name(&raw);
    }

    let lowercase = raw.to_ascii_lowercase();
    if validate_service_name(&lowercase).is_ok() {
        common::assert_valid_service_name(&lowercase);
    }
});
