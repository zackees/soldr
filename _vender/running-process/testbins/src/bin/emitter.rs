//! Test binary that prints "tick N\n" every ~50 ms forever.
//! Used by integration tests that need a child producing continuous
//! output (#130 M5 backlog accumulation, M5 ring-buffer overflow).

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut n: u64 = 0;
    let mut out = std::io::stdout().lock();
    loop {
        writeln!(out, "tick {n}").ok();
        out.flush().ok();
        n += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
}
