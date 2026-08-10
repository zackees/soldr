//! Test binary: emits raw ANSI clear+home + 10 `COUNTER: N` lines with
//! cursor-move escapes. Used by the #150 PTY-passthrough integration
//! tests to verify byte-exact ANSI propagation through ConPTY (with
//! `PSEUDOCONSOLE_PASSTHROUGH_MODE` enabled) to the daemon ring
//! buffer and the in-process PTY reader.
//!
//! The sequence written per tick:
//!   `\x1b[1;1HCOUNTER: {n}\r\n`
//! plus a one-shot clear at startup:
//!   `\x1b[H\x1b[2J`
//!
//! The 50 ms sleep between ticks gives the PTY plumbing room to
//! observe each chunk independently — without it, ConPTY can
//! coalesce buffered output and skew tests that look for
//! intermediate states.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let mut out = io::stdout().lock();
    // Cursor home + erase screen so the test reader sees a clean
    // surface before the counter ticks start.
    out.write_all(b"\x1b[H\x1b[2J").expect("write clear");
    out.flush().expect("flush clear");
    for n in 0..10 {
        // Cursor to row 1 col 1, then write the counter line.
        write!(out, "\x1b[1;1HCOUNTER: {n}\r\n").expect("write tick");
        out.flush().expect("flush tick");
        thread::sleep(Duration::from_millis(50));
    }
}
