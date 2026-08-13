//! Host OS, architecture, and environment/libc facts.

pub use crate::platform_imp::host::facts::{
    arch, info, libc, max_path, os, triple, HostArch, HostInfo, HostLibc, HostOs,
};
