//! Test binary: prints its own PID to stdout, flushes, and sleeps forever.
//! Used by containment integration tests.

use std::io::Write;

fn main() {
    let pid = std::process::id();
    println!("PID={pid}");
    std::io::stdout().flush().unwrap();
    // Sleep forever — expect to be killed by the test harness.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
