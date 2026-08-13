//! Host-neutral machine-facts facade.
//!
//! Owns raw facts and probes about the machine *running* Soldr: host OS,
//! architecture, environment/libc, and OS version; physical CPU topology;
//! host memory/process pressure; home and runtime directory discovery;
//! current-user identity and elevation; and host-security-product mechanics
//! needed by neutral callers. `TargetTriple::host()` consumes these facts —
//! this crate never constructs or depends on `TargetTriple`.

pub mod dirs;
