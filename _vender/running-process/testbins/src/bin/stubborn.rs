//! Test binary that ignores POSIX SIGTERM and prints a ready marker.
//! Used by the #130 M4 stubborn-child test to verify the daemon
//! escalates to a hard kill when the soft signal fails to terminate
//! the child within the grace window.
//!
//! On Windows the soft-signal path is a no-op (until the separate
//! CTRL_BREAK_EVENT follow-up lands), so this binary just sleeps
//! forever — the daemon's hard-kill schedule still reaps it cleanly.

use std::io::Write;

fn main() {
    #[cfg(unix)]
    {
        // SIG_IGN for SIGTERM. The child will need to be SIGKILLed
        // to actually terminate.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    println!("STUBBORN_READY");
    std::io::stdout().flush().ok();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
