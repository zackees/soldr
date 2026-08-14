//! Windows fs implementation: contention errors.

use std::io;

/// True when `error` is a lock/sharing collision.
///
/// `std` does not normalize Windows lock errors to
/// [`io::ErrorKind::WouldBlock`]; `LockFileEx` reports raw
/// `ERROR_LOCK_VIOLATION` (33) and nonblocking sharing collisions surface
/// as `ERROR_SHARING_VIOLATION` (32).
pub fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_lock_contention_recognizes_raw_errors() {
        assert!(is_lock_contention(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
        // LockFileEx reports ERROR_LOCK_VIOLATION (33); sharing
        // collisions surface as ERROR_SHARING_VIOLATION (32). std does
        // not normalize either to WouldBlock on Windows.
        assert!(is_lock_contention(&io::Error::from_raw_os_error(33)));
        assert!(is_lock_contention(&io::Error::from_raw_os_error(32)));
        assert!(!is_lock_contention(&io::Error::from(
            io::ErrorKind::NotFound
        )));
    }
}
