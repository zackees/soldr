//! Owner-only, writable, and executable permission primitives, plus
//! archived-mode restoration (archive traversal stays with the caller).

pub use crate::platform_imp::fs::permissions::{
    make_executable, make_executable_from, make_private, make_writable, restore_mode,
};
