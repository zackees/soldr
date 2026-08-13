//! Host-neutral synchronous broker-control transport with bounded I/O.

/// Apply request-phase receive and send deadlines after route negotiation.
pub use crate::platform_imp::ipc::control::configure_timeouts;
/// Connect to the broker control endpoint within the supplied route deadline.
pub use crate::platform_imp::ipc::control::connect;
/// The selected host's synchronous control stream.
pub use crate::platform_imp::ipc::control::ControlStream;
