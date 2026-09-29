//! Host OS, architecture, and environment/libc facts.

pub use crate::platform_imp::host::facts::{
    arch, glibc_version, info, libc, max_path, os, os_version, path_list_separator, triple,
    HostArch, HostInfo, HostLibc, HostOs,
};
