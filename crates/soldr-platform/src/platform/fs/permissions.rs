//! Owner-only, writable, and executable permission primitives, plus
//! archived-mode restoration (archive traversal stays with the caller).

pub use crate::platform_imp::fs::permissions::restore_mode;
