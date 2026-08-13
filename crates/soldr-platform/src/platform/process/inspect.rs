//! PID liveness, zombie state, and running-process image lookup.

pub use crate::platform_imp::process::inspect::{
    console_attached, executable_path, executable_path_matches, executable_stem_matches,
    holders_under, is_alive, is_zombie, ProcessHolder,
};
