//! Manual validation harness for the Unix hang-dump path.
//!
//! Ignored by default because the watchdog firing `std::process::exit(1)`s
//! the test binary — by design this test "fails". Run it by hand to verify
//! that a deliberately hung test produces per-thread backtraces:
//!
//! ```sh
//! cargo test -p test-watchdog --test hang_dump -- --ignored --nocapture
//! ```
//!
//! Expected stderr: the watchdog message, the gdb/lldb invocation, and an
//! all-thread backtrace listing that includes this test's
//! `deliberately-hung-worker` thread blocked in `sleep`.

use std::time::Duration;

#[test]
#[ignore = "deliberately hangs so the watchdog fires and exit(1)s; run manually"]
fn deliberate_hang_dumps_all_thread_stacks() {
    let _wd = test_watchdog::install(
        Duration::from_secs(3),
        "deliberate_hang_dumps_all_thread_stacks is (intentionally) hung",
        None,
    );
    // A named worker thread blocked in a syscall, to prove the dump is
    // out-of-process and covers non-cooperative threads.
    std::thread::Builder::new()
        .name("deliberately-hung-worker".to_string())
        .spawn(|| std::thread::sleep(Duration::from_secs(600)))
        .expect("spawn worker");
    std::thread::sleep(Duration::from_secs(600));
}
