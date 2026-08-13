//! Atomic replacement and open/running-image retirement.

pub use crate::platform_imp::fs::replace::{atomic_replace, open_for_retire, retire_open_file};
