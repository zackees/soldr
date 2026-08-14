//! Physical-core detection (soldr#1761, final acceptance criterion).
//!
//! # Why this is not just `available_parallelism()`
//!
//! `std::thread::available_parallelism` reports *logical* CPUs. On an
//! SMT host that is double the physical core count, and sizing a
//! compile pool from it oversubscribes the machine: the reporting
//! Ryzen 3700X (8 cores / 16 threads) ran 15 concurrent rustc, which
//! saturates every hardware thread and leaves nothing for the
//! interactive session that started the build.
//!
//! [`super::jobs::default_compile_jobs`] previously deferred this,
//! noting that a blanket "assume SMT, halve it" rule would penalize a
//! genuinely non-SMT host — a 16-core machine with no SMT would be
//! capped for no reason. Reading the real topology removes the
//! guesswork, so the discount applies only where SMT actually exists.
//!
//! Returning `Option` is deliberate: every backend here can fail
//! (containers hide sysfs, an API can be unavailable), and the caller
//! must degrade to the logical-CPU behavior rather than invent a
//! number. A wrong core count is worse than no core count.

/// Physical CPU cores on this machine, or `None` when the platform's
/// topology could not be read.
///
/// Memoized: the daemon asks once at startup, but the backends spawn a
/// subprocess (macOS) or walk sysfs (Linux), and neither cost should be
/// repeated if a caller ever moves this onto a warmer path.
pub fn physical_cores() -> Option<usize> {
    // The per-host detection (Linux sysfs/cpuinfo, macOS sysctl,
    // Windows GetLogicalProcessorInformationEx) and its memoization
    // live in the platform crate's host resources.
    crate::platform::host::resources::physical_cores()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_cores_are_plausible_or_absent() {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // Not asserting an exact number — this runs on unknown CI
        // hardware. The invariants that must hold anywhere are that a
        // reported count is positive and never exceeds the logical CPU
        // count, since every physical core carries at least one thread.
        if let Some(cores) = physical_cores() {
            assert!(cores > 0, "a reported core count must be positive");
            assert!(
                cores <= logical,
                "physical cores ({cores}) cannot exceed logical CPUs ({logical})"
            );
        }
    }

    #[test]
    fn physical_cores_is_stable_across_calls() {
        assert_eq!(physical_cores(), physical_cores());
    }
}
