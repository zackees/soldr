//! A real CPU profile of a real hot loop (S15 / #644).
//!
//! The unit tests in `src/profile/tests.rs` drive the pipeline from
//! synthesized samples, which proves the folding and the encoders. This drives
//! it from the actual sampler against actual running threads — the part that
//! cannot be faked, and the part where "does it capture multiple threads at
//! all" is answered.
//!
//! Sampling suspends sibling threads, so this test is deliberately short and
//! its assertions are about *shape* (multi-thread coverage, bounded cost,
//! exports produced) rather than exact sample counts, which no wall-clock
//! sampler can promise on a loaded CI runner.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use running_process_probe_daemon::profile::export::firefox::to_firefox_value;
use running_process_probe_daemon::profile::export::{all, to_collapsed, to_pprof_bytes};
use running_process_probe_daemon::profile::{
    ModuleResolver, ProfileRequest, ProfileSession, MAX_DURATION,
};

/// Keeps a few threads busy for the life of the guard.
struct HotLoad {
    stop: Arc<AtomicBool>,
    spins: Arc<AtomicU64>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl HotLoad {
    fn start(count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let spins = Arc::new(AtomicU64::new(0));
        let threads = (0..count)
            .map(|index| {
                let stop = Arc::clone(&stop);
                let spins = Arc::clone(&spins);
                std::thread::Builder::new()
                    .name(format!("hot-{index}"))
                    .spawn(move || spin_hot(&stop, &spins))
                    .expect("spawn hot thread")
            })
            .collect();
        Self {
            stop,
            spins,
            threads,
        }
    }
}

impl Drop for HotLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// The function a profile of this test should be dominated by.
///
/// `#[inline(never)]` so it survives as its own frame; inlined into its caller
/// it would be attributed to the caller and the test would be asserting
/// something the optimizer chose rather than something the profiler measured.
#[inline(never)]
fn spin_hot(stop: &AtomicBool, spins: &AtomicU64) {
    let mut accumulator: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        for i in 0..10_000u64 {
            accumulator = accumulator.wrapping_add(i).wrapping_mul(2_654_435_761);
        }
        spins.fetch_add(accumulator | 1, Ordering::Relaxed);
    }
}

/// Profile this process for `millis` while several threads spin.
fn profile_hot_load(millis: u64) -> Option<running_process_probe_daemon::profile::SessionResult> {
    let load = HotLoad::start(3);
    // Let the threads actually get going, so the first samples are not all of
    // thread startup.
    std::thread::sleep(Duration::from_millis(50));

    let session = ProfileSession::new(ProfileRequest {
        hz: 200,
        duration: Duration::from_millis(millis),
    });
    let metrics = session.run();

    // Nothing captured means this platform has no cooperative capture backend
    // yet. Skip rather than fail: the pipeline is still correct, and a
    // platform gap is not a regression in it.
    if metrics.samples_captured == 0 {
        drop(load);
        return None;
    }

    let mut resolver = ModuleResolver::for_current_process().expect("module list");
    let result = session.resolve(&mut resolver, metrics);
    assert!(load.spins.load(Ordering::Relaxed) > 0, "the load never ran");
    drop(load);
    Some(result)
}

#[test]
fn a_running_process_yields_multi_thread_samples() {
    let Some(result) = profile_hot_load(400) else {
        eprintln!("skipping: no cooperative capture backend on this platform");
        return;
    };

    assert!(result.metrics.samples_captured > 0);
    assert!(
        result.metrics.threads_seen > 1,
        "a profile that saw one thread of a multi-threaded program is not a \
         profile of that program: {:?}",
        result.metrics
    );
    assert!(result.metrics.threads_at_start > 0);
    assert!(
        result.metrics.thread_coverage() > 0.0,
        "coverage must be reported so a partial profile is visibly partial"
    );
}

#[test]
fn the_cost_the_profiler_imposes_is_measured_and_reported() {
    // Not "is small" — measured. A profiler that hides its own cost lets an
    // operator misread an overhead-shaped profile as a program-shaped one.
    let Some(result) = profile_hot_load(400) else {
        eprintln!("skipping: no cooperative capture backend on this platform");
        return;
    };
    assert!(result.metrics.duration_nanos > 0);
    assert!(
        result.metrics.overhead_ratio() < 1.0,
        "the target cannot have been suspended for longer than the session ran"
    );
    assert_eq!(
        result.metrics.samples_dropped, 0,
        "a short session must not overflow the ring"
    );
    assert_eq!(result.metrics.fidelity(), 1.0);
}

#[test]
fn a_real_profile_exports_to_all_three_formats() {
    let Some(result) = profile_hot_load(300) else {
        eprintln!("skipping: no cooperative capture backend on this platform");
        return;
    };

    let exports = all(&result).expect("exports");

    // Collapsed: one line per unique stack, `frames <count>`.
    let first = exports.collapsed.lines().next().expect("a folded stack");
    let (frames, count) = first.rsplit_once(' ').expect("a count");
    assert!(!frames.is_empty());
    assert!(count.parse::<u64>().expect("numeric count") > 0);

    // pprof: decodes, and its string table honours the spec's index-0 rule.
    let profile =
        <running_process_probe_daemon::profile::pprof::Profile as prost::Message>::decode(
            to_pprof_bytes(&result).as_slice(),
        )
        .expect("pprof must decode");
    assert_eq!(profile.string_table[0], "");
    assert!(!profile.sample.is_empty());
    assert_eq!(profile.duration_nanos, result.metrics.duration_nanos as i64);

    // Gzipped, as the `.pb.gz` convention requires.
    assert_eq!(&exports.pprof_gzip[..2], &[0x1f, 0x8b]);

    // Firefox: parses, and its tables are self-consistent.
    let firefox: serde_json::Value =
        serde_json::from_str(&exports.firefox_json).expect("firefox json");
    let thread = &firefox["threads"][0];
    assert_eq!(
        thread["samples"]["length"].as_u64(),
        thread["samples"]["stack"]
            .as_array()
            .map(|a| a.len() as u64)
    );
}

#[test]
fn samples_symbolize_after_the_sampled_threads_have_exited() {
    // The reason names are resolved from `(module, relative address)` rather
    // than looked up during capture: this is exactly the case that matters,
    // because a process that died mid-session is the one you most want a
    // profile of.
    let load = HotLoad::start(2);
    std::thread::sleep(Duration::from_millis(50));

    let session = ProfileSession::new(ProfileRequest {
        hz: 200,
        duration: Duration::from_millis(200),
    });
    let metrics = session.run();
    if metrics.samples_captured == 0 {
        eprintln!("skipping: no cooperative capture backend on this platform");
        return;
    }

    // Threads gone, ring still full of raw addresses.
    drop(load);

    let mut resolver = ModuleResolver::for_current_process().expect("module list");
    let result = session.resolve(&mut resolver, metrics);

    assert!(!result.samples.is_empty());
    assert!(
        result
            .samples
            .iter()
            .any(|sample| sample.frames.iter().any(|frame| !frame.module.is_empty())),
        "no frame was attributed to a module, so nothing symbolized after exit"
    );
    assert!(!to_collapsed(&result).is_empty());
}

#[test]
fn a_session_longer_than_the_cap_is_bounded_by_it() {
    // Asserted on the clamp rather than by running for a minute: the bound is
    // a property of the request, and a test that waited it out would add sixty
    // seconds to every CI run to learn the same fact.
    let session = ProfileSession::new(ProfileRequest {
        hz: 99,
        duration: Duration::from_secs(3600),
    });
    assert_eq!(session.request().duration, MAX_DURATION);

    let firefox =
        to_firefox_value(&running_process_probe_daemon::profile::SessionResult::default());
    assert!(firefox["threads"][0]["samples"]["length"].as_u64() == Some(0));
}
