//! Normalization of OS-specific lock/contention errors.
//!
//! `std` normalizes nonblocking lock collisions to
//! [`std::io::ErrorKind::WouldBlock`] on Unix, but not on Windows, where
//! `LockFileEx` reports raw `ERROR_LOCK_VIOLATION` (33) / `ERROR_SHARING_VIOLATION`
//! (32) and file locking reports `ERROR_SHARING_VIOLATION`. Callers compare
//! lock failures with this predicate instead of raw OS error codes.

pub use crate::platform_imp::fs::contention::is_lock_contention;
