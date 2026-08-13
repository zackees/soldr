//! Host OS, architecture, and environment/libc facts.

pub use crate::platform_imp::host::facts::{
    arch, info, libc, os, HostArch, HostInfo, HostLibc, HostOs,
};
