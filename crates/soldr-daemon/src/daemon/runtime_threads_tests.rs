//! soldr#3210: a blocking-pool thread that spawned a compiler must not retire.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Sets its flag when the thread that owns it exits.
struct ThreadExitProbe(Arc<AtomicBool>);

impl Drop for ThreadExitProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

thread_local! {
    static PROBE: std::cell::RefCell<Option<ThreadExitProbe>> = const { std::cell::RefCell::new(None) };
}

/// Run one blocking task on `builder`'s runtime and report whether its pool
/// thread exited within `wait`. The runtime stays alive for the whole wait,
/// so an exit can only be keep-alive retirement, never shutdown.
fn pool_thread_exits_within(builder: &mut tokio::runtime::Builder, wait: Duration) -> bool {
    let runtime = builder.enable_all().build().expect("tokio runtime");
    let exited = Arc::new(AtomicBool::new(false));
    let armed = exited.clone();
    // Spawn through the runtime handle: `tokio::task::spawn_blocking` would
    // need an entered runtime context, which `block_on` only provides later.
    let task = runtime.spawn_blocking(move || {
        PROBE.with(|probe| *probe.borrow_mut() = Some(ThreadExitProbe(armed)));
    });
    runtime.block_on(task).expect("blocking task");
    std::thread::sleep(wait);
    let result = exited.load(Ordering::SeqCst);
    drop(runtime);
    result
}

/// Control: with a short keep-alive, an idle pool thread really does exit
/// while the runtime is still running. This is the retirement that delivers
/// `PR_SET_PDEATHSIG` to a compiler the thread spawned.
#[test]
fn a_short_keep_alive_retires_an_idle_pool_thread() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(1)
        .thread_keep_alive(Duration::from_millis(50));
    assert!(pool_thread_exits_within(
        &mut builder,
        Duration::from_millis(750)
    ));
}

/// The daemon policy overrides a short keep-alive, so the same idle thread
/// survives.
#[test]
fn the_daemon_policy_keeps_an_idle_pool_thread_alive() {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(1)
        .thread_keep_alive(Duration::from_millis(50));
    keep_threads_for_daemon_lifetime(&mut builder);
    assert!(!pool_thread_exits_within(
        &mut builder,
        Duration::from_millis(750)
    ));
}

/// "Longer than the daemon lives" has to beat every daemon lifetime bound,
/// including an idle daemon that never times out.
#[test]
fn the_keep_alive_outlasts_any_daemon() {
    assert!(DAEMON_THREAD_KEEP_ALIVE >= Duration::from_secs(100 * 365 * 24 * 3600));
}
