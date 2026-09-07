//! soldr#3158: the guarantees the broker's shutdown depends on.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::ShutdownSignal;

const WAKE_BUDGET: Duration = Duration::from_secs(5);

/// The soldr#3158 regression, at the primitive level: N loops park on one
/// signal, one `request()` must wake **all** of them. `Notify::notify_one()`
/// wakes exactly one and leaves the rest parked forever, which is how a
/// cooperative broker stop came to burn its full drain deadline on every run.
#[tokio::test]
async fn request_wakes_every_waiter_not_just_one() {
    const WAITERS: usize = 4;

    let signal = Arc::new(ShutdownSignal::default());
    let woken = Arc::new(AtomicUsize::new(0));

    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| {
            let signal = Arc::clone(&signal);
            let woken = Arc::clone(&woken);
            tokio::spawn(async move {
                signal.wait().await;
                woken.fetch_add(1, Ordering::Release);
            })
        })
        .collect();

    // Let every waiter register before the request lands.
    tokio::task::yield_now().await;
    signal.request();

    for waiter in waiters {
        tokio::time::timeout(WAKE_BUDGET, waiter)
            .await
            .expect("every waiter must wake from one request")
            .expect("waiter task must not panic");
    }
    assert_eq!(woken.load(Ordering::Acquire), WAITERS);
}

/// A request that lands before anyone waits must not be lost: `wait()` reads
/// the latched flag rather than parking on a notification that already fired.
#[tokio::test]
async fn request_before_any_waiter_is_latched() {
    let signal = ShutdownSignal::default();
    assert!(!signal.is_requested());
    signal.request();
    assert!(signal.is_requested());

    tokio::time::timeout(WAKE_BUDGET, signal.wait())
        .await
        .expect("a latched request must resolve wait() immediately");
}

/// `wait()` is used inside `tokio::select!`, so it is dropped every time a
/// sibling branch wins. A drop must not consume the request.
#[tokio::test]
async fn wait_is_cancel_safe_inside_select() {
    let signal = Arc::new(ShutdownSignal::default());

    // Drop a `wait()` future that raced the request: `notify_waiters()` may
    // have already marked it notified, and a notification consumed by a
    // dropped future is gone. The latched flag is what makes the next pass
    // observe the request anyway.
    let racing = Arc::clone(&signal);
    let select_pass = tokio::spawn(async move {
        tokio::select! {
            () = racing.wait() => "shutdown",
            () = tokio::time::sleep(Duration::from_millis(1)) => "sibling",
        }
    });
    signal.request();
    let _ = tokio::time::timeout(WAKE_BUDGET, select_pass)
        .await
        .expect("select pass must finish");

    // The next pass — the loop's next iteration — must still see it.
    tokio::time::timeout(WAKE_BUDGET, signal.wait())
        .await
        .expect("a dropped wait() must not consume the shutdown request");
}

/// `request()` is called from a connection handler that cannot know whether
/// another handler already asked; repeating it must stay harmless.
#[tokio::test]
async fn request_is_idempotent() {
    let signal = ShutdownSignal::default();
    signal.request();
    signal.request();
    signal.request();
    tokio::time::timeout(WAKE_BUDGET, signal.wait())
        .await
        .expect("repeated requests must leave the signal resolved");
}
