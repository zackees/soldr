//! Physical CPU topology and host memory/process pressure.

pub use crate::platform_imp::host::resources::{
    cgroup_v2_dir, commit_charge_mb, physical_cores, process_table,
};
