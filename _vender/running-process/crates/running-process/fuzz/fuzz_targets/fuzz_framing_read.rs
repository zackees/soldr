#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use running_process::broker::protocol::framing::read_frame_with_cap;

mod common;

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    if let Ok(body) = read_frame_with_cap(&mut cursor, common::FUZZ_FRAME_READ_CAP) {
        assert!(
            body.len() <= common::FUZZ_FRAME_READ_CAP,
            "read_frame_with_cap returned a body over its cap"
        );
    }
});
