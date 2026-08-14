//! macOS fs implementation: contention errors.

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

    #[test]
    fn macos_lock_contention_uses_normalized_kind() {
        assert!(is_lock_contention(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
        // Raw Windows codes carry no meaning on macOS.
        assert!(!is_lock_contention(&io::Error::from_raw_os_error(33)));
    }
}
