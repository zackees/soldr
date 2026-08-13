//! Process-tree termination and escalation.

pub use crate::platform_imp::process::terminate::{
    signal_pid, terminate_pid, terminate_tree, TreeKill,
};
