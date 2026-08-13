//! Windows exposes no procfs CPU accounting; the caller falls back to
//! live output as progress.

/// No CPU-ticks probe on Windows; callers treat output as progress.
pub fn process_tree_cpu_ticks(_root_pid: u32) -> Option<u64> {
    None
}
