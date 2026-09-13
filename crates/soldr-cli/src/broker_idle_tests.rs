//! soldr#3193: a broker nobody uses stands itself down.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

const TICK: Duration = Duration::from_millis(2);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// Run the stand-down against a scripted idleness sequence for at most
/// `budget`. Returns whether shutdown was requested and how many samples ran.
fn standdown_for(
    budget: Duration,
    window: Duration,
    mut script: impl FnMut(usize) -> bool,
) -> (bool, usize) {
    let shutdown = Arc::new(ShutdownSignal::default());
    let samples = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&samples);
    let task = run_idle_standdown(Arc::clone(&shutdown), TICK, window, move || {
        script(counter.fetch_add(1, Ordering::Relaxed))
    });
    let _ = runtime().block_on(async { tokio::time::timeout(budget, task).await });
    (shutdown.is_requested(), samples.load(Ordering::Relaxed))
}

#[test]
fn a_continuously_idle_broker_stands_down_after_the_window() {
    let (requested, samples) =
        standdown_for(Duration::from_secs(5), Duration::from_millis(20), |_| true);
    assert!(requested);
    assert!(
        samples >= 2,
        "the window spans several samples, saw {samples}"
    );
}

#[test]
fn a_busy_sample_restarts_the_clock() {
    // Busy every other tick: never idle for a whole window.
    let (requested, _) =
        standdown_for(Duration::from_millis(150), Duration::from_millis(20), |n| {
            n % 2 == 1
        });
    assert!(!requested);
}

#[test]
fn a_busy_broker_never_stands_down() {
    let (requested, samples) =
        standdown_for(Duration::from_millis(60), Duration::from_millis(4), |_| {
            false
        });
    assert!(!requested);
    assert!(samples >= 3);
}

#[test]
fn an_external_shutdown_ends_the_task() {
    let shutdown = Arc::new(ShutdownSignal::default());
    shutdown.request();
    let task = run_idle_standdown(Arc::clone(&shutdown), TICK, Duration::from_secs(60), || {
        true
    });
    runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("returns promptly once shutdown is requested");
    });
}

#[test]
fn the_window_override_parses_zero_as_disabled_and_junk_as_default() {
    assert_eq!(idle_exit_window_for(None), Some(DEFAULT_IDLE_EXIT));
    assert_eq!(idle_exit_window_for(Some("")), Some(DEFAULT_IDLE_EXIT));
    assert_eq!(idle_exit_window_for(Some("0")), None);
    assert_eq!(
        idle_exit_window_for(Some(" 90 ")),
        Some(Duration::from_secs(90))
    );
    assert_eq!(idle_exit_window_for(Some("soon")), Some(DEFAULT_IDLE_EXIT));
}
