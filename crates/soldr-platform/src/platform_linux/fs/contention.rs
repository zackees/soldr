//! Linux fs implementation: contention errors.

use std::io;

/// True when `error` is a lock collision. `std` normalizes nonblocking
/// lock failures to [`io::ErrorKind::WouldBlock`] on Unix, so no raw
/// error codes are needed.
pub fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn linux_lock_contention_uses_normalized_kind() {
        assert!(is_lock_contention(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
        // Raw Windows codes carry no meaning on Linux.
        assert!(!is_lock_contention(&io::Error::from_raw_os_error(33)));
    }
}
