//! Exit/signal interpretation.

pub use crate::platform_imp::process::exit::{
    exit_status_from_code, is_init_failure, is_signal_termination, termination_kind,
    TerminationKind,
};
