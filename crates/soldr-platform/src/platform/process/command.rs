//! Command configuration: console suppression, process-group setup, and
//! argv[0] identity.

pub use crate::platform_imp::process::command::{arg0, configure_process_group, suppress_console};
