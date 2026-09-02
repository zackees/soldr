//! Unit tests for owner-death reaping of broker routes (soldr#3054).

use super::*;
use std::collections::BTreeSet;

/// A fixture asks for its own route exactly once and then dies. Nothing else
/// will ever ask for that route again, so it must not survive the grace
/// window. This is the leak the whole module exists to close: 63 daemons in
/// this state after one run of the suite.
#[test]
fn a_route_whose_only_requester_died_is_reaped_after_the_grace_window() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", 4242, start);

    let dead = |_pid: u32| false;

    let during = ownership.sweep(start, DEFAULT_GRACE, dead);
    assert_eq!(
        during,
        vec![("soldr-daemon-fixture".into(), RouteVerdict::Draining)]
    );

    let after = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, dead);
    assert_eq!(
        after,
        vec![("soldr-daemon-fixture".into(), RouteVerdict::Reap)]
    );
}

/// The canonical route for the user's own root is asked for by every build,
/// and each build's process exits when it finishes. Reaping on "the last
/// requester exited" alone would therefore kill the daemon the user actually
/// wants resident, and a cold daemon start pays a full executable image hash
/// (soldr#2517). A new requester inside the window must reset it.
#[test]
fn a_new_requester_inside_the_window_keeps_the_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-canonical", 100, start);

    let only_200_lives = |pid: u32| pid == 200;

    // The first build's process is gone; the route starts draining.
    let draining = ownership.sweep(start, DEFAULT_GRACE, |_| false);
    assert_eq!(draining[0].1, RouteVerdict::Draining);

    // The next build arrives before the window elapses.
    ownership.record_request("soldr-daemon-canonical", 200, start);
    let after_window = ownership.sweep(start + DEFAULT_GRACE * 2, DEFAULT_GRACE, only_200_lives);
    assert_eq!(
        after_window,
        vec![("soldr-daemon-canonical".into(), RouteVerdict::Live)],
        "a live requester outranks any amount of elapsed time"
    );
}

/// The window is measured from when the route went empty, not from the sweep
/// that noticed it. A broker that sweeps every 30s against a 120s grace would
/// otherwise restart the clock on every tick and never reap anything.
#[test]
fn the_grace_window_runs_from_when_the_route_emptied_not_from_the_last_sweep() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", 7, start);
    let dead = |_pid: u32| false;

    for tick in 1..4 {
        let verdicts = ownership.sweep(start + Duration::from_secs(30 * tick), DEFAULT_GRACE, dead);
        assert_eq!(verdicts[0].1, RouteVerdict::Draining, "tick {tick}");
    }
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, dead);
    assert_eq!(verdicts[0].1, RouteVerdict::Reap);
}

/// A route may be shared. One requester dying is not the route dying, and the
/// dead one must still be dropped from the set so it cannot keep the route
/// alive forever after its PID is reused by something unrelated.
#[test]
fn a_dead_requester_is_dropped_while_a_live_one_holds_the_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-shared", 11, start);
    ownership.record_request("soldr-daemon-shared", 22, start);
    assert_eq!(ownership.owners_of("soldr-daemon-shared").unwrap().len(), 2);

    let verdicts = ownership.sweep(Instant::now(), DEFAULT_GRACE, |pid| pid == 22);
    assert_eq!(verdicts[0].1, RouteVerdict::Live);
    assert_eq!(
        ownership.owners_of("soldr-daemon-shared").unwrap().len(),
        1,
        "the exited requester must not linger in the set"
    );
}

/// Recording the same requester twice is the normal case -- one process runs
/// several builds -- and must not inflate the set.
#[test]
fn the_same_requester_is_recorded_once() {
    let mut ownership = RouteOwnership::new();
    for _ in 0..5 {
        ownership.record_request("soldr-daemon-fixture", 99, Instant::now());
    }
    assert_eq!(
        ownership.owners_of("soldr-daemon-fixture").unwrap().len(),
        1
    );
}

/// Routes are independent: reaping one must not disturb another.
#[test]
fn routes_are_classified_independently() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-a", 1, start);
    ownership.record_request("soldr-daemon-b", 2, start);

    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, |pid| pid == 2);
    let by_name: BTreeSet<(String, RouteVerdict)> = verdicts.into_iter().collect();
    assert!(by_name.contains(&("soldr-daemon-a".to_string(), RouteVerdict::Reap)));
    assert!(by_name.contains(&("soldr-daemon-b".to_string(), RouteVerdict::Live)));
}

/// After the daemon is gone the route must leave the table, or a broker that
/// runs for days accumulates an entry per fixture that ever existed.
#[test]
fn a_reaped_route_is_forgotten() {
    let mut ownership = RouteOwnership::new();
    ownership.record_request("soldr-daemon-fixture", 5, Instant::now());
    assert_eq!(ownership.len(), 1);
    ownership.forget("soldr-daemon-fixture");
    assert_eq!(ownership.len(), 0);
    assert!(ownership.owners_of("soldr-daemon-fixture").is_none());
}

/// The production probe must agree with reality on this host, in both
/// directions. This is the one test that touches the real platform layer, and
/// it is why the module takes `is_alive` as a parameter everywhere else.
#[test]
fn the_production_probe_answers_for_real_processes() {
    assert!(
        requester_is_alive(std::process::id()),
        "this process is its own proof of liveness"
    );
    // Not a pid this host can be running: `pid_max` is far below this, and a
    // handle-based probe cannot be fooled by the signedness trap that made
    // `kill(u32::MAX as pid_t, 0)` succeed as `kill(-1, 0)`.
    assert!(!requester_is_alive(u32::MAX));
}

/// The reap decision must survive a requester whose PID is reused.
///
/// The set holds numbers, so a probe that only asks "is some process with
/// this number alive" would keep a route alive forever once the kernel
/// recycled the PID onto something unrelated. `requester_is_alive` opens a
/// handle instead, which is why the production path cannot drift into that
/// state; this test pins the decision logic that sits on top of it, using an
/// oracle that reports the reuse honestly.
#[test]
fn a_recycled_pid_does_not_resurrect_a_route() {
    let mut ownership = RouteOwnership::new();
    let start = Instant::now();
    ownership.record_request("soldr-daemon-fixture", 1234, start);

    // The original owner is gone. A handle-based probe says so even though a
    // number-based one would see the unrelated process now holding 1234.
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, |_| false);
    assert_eq!(verdicts[0].1, RouteVerdict::Reap);

    // And the route is forgotten, so a later request from the recycled PID
    // starts a fresh route rather than re-entering the old one's window.
    ownership.forget("soldr-daemon-fixture");
    ownership.record_request("soldr-daemon-fixture", 1234, start + DEFAULT_GRACE);
    let verdicts = ownership.sweep(start + DEFAULT_GRACE, DEFAULT_GRACE, |pid| pid == 1234);
    assert_eq!(verdicts[0].1, RouteVerdict::Live);
}

/// An empty table is the steady state on a machine doing nothing, and must
/// not produce work or panic.
#[test]
fn sweeping_an_empty_table_is_a_no_op() {
    let mut ownership = RouteOwnership::new();
    assert!(ownership
        .sweep(Instant::now(), DEFAULT_GRACE, |_| true)
        .is_empty());
    assert_eq!(ownership.len(), 0);
}

/// The sweep interval must leave room inside the grace window, or a route
/// reaches `Reap` and then waits most of another window to be noticed.
#[test]
fn the_sweep_interval_fits_inside_the_grace_window() {
    assert!(
        crate::broker_server::REAP_SWEEP_INTERVAL < DEFAULT_GRACE,
        "a sweep slower than the window makes the window meaningless"
    );
}
