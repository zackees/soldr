//! soldr#2760 front-door walk-pool regression test.
//!
//! Split out of the sibling `tests.rs` for the same reason
//! `log_summary_tests.rs` was (soldr#2493): that file is already past the
//! 1,500-line ceiling, so the ratchet correctly refuses to let it grow. A new
//! module is what the ratchet asks for, and adding one -- rather than renaming
//! the hot file -- cannot conflict with in-flight branches.

use super::*;

// soldr#2760, front-door half: the target-dir scrub walk used jwalk's default
// parallelism, which aborts after one second when the ambient rayon pool
// cannot serve it. That is not hypothetical here -- this suite hit it on a
// loaded machine, as
//
//   scrub idle target: Io(Custom { kind: Other,
//                                  error: Error { depth: 0, inner: ThreadpoolBusy } })
//
// while the same test passed in isolation, which is what a load-dependent
// abort looks like.
//
// A one-thread ambient pool makes it deterministic rather than load-dependent:
// the calling closure is the only worker, so a walk that spawns onto that pool
// can never be served. `RayonNewPool` reports `timeout() == None`, so it
// cannot abort at all.
#[test]
fn scrub_survives_a_saturated_ambient_pool() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    let fingerprint = target.join("debug/.fingerprint/demo-123");
    std::fs::create_dir_all(&fingerprint).expect("create fingerprint directory");
    std::fs::write(
        fingerprint.join("output-lib-demo"),
        b"warning: diagnostic\n",
    )
    .expect("write fixture");

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("build one-thread pool");

    let outcome = pool
        .install(|| scrub_cached_fallback_diagnostics_once(&target))
        .expect("scrub must not abort when the ambient rayon pool is saturated");

    assert!(matches!(
        outcome,
        FallbackOutputScrub::Complete(_) | FallbackOutputScrub::AlreadyDone
    ));
}
