#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use running_process::broker::backend_lifecycle::probe::read_probe_frame;

mod common;

const PROBE_FRAME_HEADER_BYTES: usize = 1 + 4;

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    if let Ok(body) = read_probe_frame(&mut cursor) {
        assert!(
            body.len() <= common::MAX_PROTO_INPUT_BYTES,
            "read_probe_frame returned a body over the v1 frame cap"
        );
        assert_eq!(
            body.as_slice(),
            &data[PROBE_FRAME_HEADER_BYTES..PROBE_FRAME_HEADER_BYTES + body.len()],
            "read_probe_frame body must be the bytes after the 5-byte header"
        );
    }
});
