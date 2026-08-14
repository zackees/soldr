//! Unit tests for [`crate::cache_lib::redb_lock`]: the contended-open
//! retry loop, the circuit breaker, and the forensic contention records.
//! Lives in a sibling file referenced via `#[path]` so `redb_lock.rs`
//! stays under the 1000-LOC ceiling.

use super::*;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

const LOCK_HOLDER_DIR_ENV: &str = "SOLDR_REDB_LOCK_HOLDER_DIR";
// Plain libtest runs these cases concurrently inside one process, where
// they intentionally share the production state-db mutex. Nextest gives
// each case its own process and tempdir, so cross-test serialization is
// unnecessary there.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn serial_test_guard() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A policy with a caller-chosen budget and the breaker disabled, for the
/// tests that only care about the retry loop itself.
fn test_policy(timeout: Duration) -> RetryPolicy {
    RetryPolicy {
        timeout,
        initial_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(50),
        breaker_threshold: 0,
        breaker_cooloff: Duration::ZERO,
        summary_interval: Duration::ZERO,
    }
}

#[test]
fn subprocess_lock_holder() {
    let Some(dir) = std::env::var_os(LOCK_HOLDER_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let db = Database::builder()
        .create(dir.join("state.redb"))
        .expect("subprocess blocking open");
    fs::write(dir.join("ready"), b"").expect("write ready marker");

    let release = dir.join("release");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "parent did not release subprocess lock holder"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(db);
}

#[test]
fn retries_actual_subprocess_contention_until_release() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let test_name = "cache_lib::redb_lock::tests::subprocess_lock_holder";
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", test_name, "--nocapture"])
        .env(LOCK_HOLDER_DIR_ENV, dir.path())
        .spawn()
        .expect("spawn lock-holder subprocess");

    let ready = dir.path().join("ready");
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            child.try_wait().expect("poll lock-holder child").is_none(),
            "lock-holder subprocess exited before acquiring the database"
        );
        assert!(
            Instant::now() < ready_deadline,
            "lock-holder subprocess did not acquire the database"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut observed_contention = false;
    let opened = open_state_db_with_retry(
        &dir.path().join("state.redb"),
        test_policy(Duration::from_secs(5)),
        OpenIntent::Required,
        || {
            if !observed_contention {
                observed_contention = true;
                fs::write(dir.path().join("release"), b"").expect("release lock holder");
            }
        },
    );

    opened.expect("open succeeded after subprocess released database");
    assert!(
        observed_contention,
        "parent must observe subprocess contention"
    );
    assert!(
        child.wait().expect("wait for lock-holder child").success(),
        "lock-holder subprocess failed"
    );
}

#[test]
fn retries_cross_process_style_open_contention_until_release() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    // Bypass the soldr mutex to model a different process holding redb's
    // file lock.
    let blocker = Database::builder().create(&path).expect("blocking open");
    let (contended_tx, contended_rx) = mpsc::sync_channel(1);
    let worker_path = path.clone();
    // The worker's retry budget must comfortably exceed
    // OBSERVE_CONTENTION_WAIT below. The blocker is released only
    // *after* the parent observes contention, so a budget equal to
    // that wait lets the worker give up before the release it is
    // waiting for ever happens. Both were 1s, which raced on the
    // emulated aarch64-Windows lane: the parent spent most of its
    // second in `recv_timeout`, and by the time it dropped the
    // blocker the worker had already returned `Err`.
    //
    // A large budget is free on the happy path — the worker returns
    // as soon as the open succeeds, not when the budget expires.
    const WORKER_RETRY_BUDGET: Duration = Duration::from_secs(30);
    // Generous for the same reason: the emulated Windows lanes are
    // an order of magnitude slower than the host ones, and this
    // asserts *that* contention is observed, not how fast.
    const OBSERVE_CONTENTION_WAIT: Duration = Duration::from_secs(10);

    let worker = std::thread::spawn(move || {
        open_state_db_with_retry(
            &worker_path,
            test_policy(WORKER_RETRY_BUDGET),
            OpenIntent::Required,
            || {
                let _ = contended_tx.try_send(());
            },
        )
        .map(drop)
    });

    contended_rx
        .recv_timeout(OBSERVE_CONTENTION_WAIT)
        .expect("worker observed contention");
    drop(blocker);

    worker
        .join()
        .expect("worker joined")
        .expect("open succeeded after release");
}

#[test]
fn stops_retrying_after_injected_budget() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);

    let budget = Duration::from_secs(1);
    let started = Instant::now();
    let result = open_state_db_with_retry(&path, test_policy(budget), OpenIntent::Required, || {
        observed.fetch_add(1, Ordering::Relaxed);
    });
    let error = match result {
        Ok(_) => panic!("contention should outlive the retry budget"),
        Err(error) => error,
    };

    assert!(matches!(error, redb::DatabaseError::DatabaseAlreadyOpen));
    assert!(attempts.load(Ordering::Relaxed) >= 2);
    assert!(started.elapsed() >= budget);
    assert!(started.elapsed() < Duration::from_secs(3));
}

// Issue #1814: a contended open must leave a durable record even when
// nobody is watching stderr. This is the forensic half of the loud-plus-
// durable pair; the tracing half is not capturable here.
#[test]
fn contention_appends_a_durable_forensic_record() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");

    let result = open_state_db_with_retry(
        &path,
        test_policy(Duration::from_millis(80)),
        OpenIntent::BestEffort,
        || {},
    );
    assert!(result.is_err(), "blocked open must not succeed");

    let log = fs::read_to_string(dir.path().join("logs").join(CONTENTION_LOG_FILE))
        .expect("contention log must exist after a contended open");
    let line = log.lines().next().expect("at least one record");
    assert!(line.contains(r#""event":"budget-exhausted""#), "{line}");
    assert!(line.contains(r#""intent":"best_effort""#), "{line}");
    assert!(line.contains(r#""attempts":"#), "{line}");
    assert!(line.contains(r#""elapsed_ms":"#), "{line}");
    assert!(
        line.contains(&format!(r#""pid":{}"#, std::process::id())),
        "{line}"
    );
}

// Issue #1814 acceptance: no 5 s stalls on the wrapper hot path. The
// best-effort budget must give up in tens of milliseconds while the
// required budget keeps waiting out the full window.
#[test]
fn best_effort_open_gives_up_far_sooner_than_required() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");

    let started = Instant::now();
    let result = open_state_db_best_effort(&path);
    let elapsed = started.elapsed();

    assert!(
        matches!(result, Err(redb::DatabaseError::DatabaseAlreadyOpen)),
        "a held database must surface as contention, not another error"
    );
    assert!(
        elapsed >= BEST_EFFORT_OPEN_TIMEOUT,
        "best-effort open must still honor its own budget, took {elapsed:?}"
    );
    assert!(
        elapsed < OPEN_RETRY_TIMEOUT / 2,
        "best-effort open must not inherit the {OPEN_RETRY_TIMEOUT:?} \
         required budget; took {elapsed:?} (issue #1814)"
    );
}

// A wait that RESOLVED must not touch the filesystem. `report_contention`
// runs inside the `state_db_open_lock` critical section, so an append here
// serializes every contended opener behind a `create_dir_all` + write —
// which blew `build_log`'s 10 s test budget (two 5 s `Required` opens back
// to back) on the musl lane. The loud half still fires via tracing.
#[test]
fn resolved_contention_stays_io_free() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let blocker = Database::builder().create(&path).expect("blocking open");

    // Release the blocker on the first observed contention so the open
    // resolves after at least one retry.
    let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&released);
    let mut blocker = Some(blocker);
    let opened = open_state_db_with_retry(
        &path,
        test_policy(Duration::from_secs(5)),
        OpenIntent::Required,
        || {
            if !flag.swap(true, Ordering::Relaxed) {
                drop(blocker.take());
            }
        },
    );
    drop(opened.expect("open resolves once the blocker releases"));

    assert!(
        released.load(Ordering::Relaxed),
        "the test must actually have gone through the retry path"
    );
    assert!(
        !dir.path().join("logs").join(CONTENTION_LOG_FILE).exists(),
        "a resolved wait must not write a durable record from inside the \
         open critical section"
    );
}

// soldr#2224: the open counter is the mechanism two other tests use to
// assert "this path opens state.redb exactly once", so it has to count
// opens rather than calls, and it has to be immune to whatever else the
// suite is doing in parallel.
#[test]
fn the_open_counter_counts_this_threads_opens() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");

    let before = state_db_open_count();
    drop(open_state_db(&path).expect("first open"));
    drop(open_state_db(&path).expect("second open"));
    assert_eq!(state_db_open_count() - before, 2);

    // Another thread's opens are not ours.
    let other = path.clone();
    std::thread::spawn(move || drop(open_state_db(&other).expect("other-thread open")))
        .join()
        .expect("other thread");
    assert_eq!(state_db_open_count() - before, 2);
}

// Issue #2230: an opener that passed the breaker before another opener
// exhausts its budget must still fail fast after waiting on the in-process
// mutex. Otherwise a contention burst burns one full retry budget per
// already-queued caller.
#[test]
fn queued_opener_rechecks_a_newly_open_breaker() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");
    let policy = RetryPolicy {
        timeout: Duration::from_millis(150),
        initial_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(25),
        breaker_threshold: 1,
        breaker_cooloff: Duration::from_secs(1),
        summary_interval: Duration::from_secs(1),
    };

    let (contending_tx, contending_rx) = mpsc::channel();
    let leader_path = path.clone();
    let leader = std::thread::spawn(move || {
        let mut first = true;
        let result = open_state_db_with_retry(&leader_path, policy, OpenIntent::Required, || {
            if first {
                contending_tx.send(()).expect("notify contention");
                first = false;
            }
        });
        assert!(
            result.is_err(),
            "the held database must exhaust the leader budget"
        );
    });
    contending_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("leader must hold the state-db mutex while retrying");

    let follower_attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::clone(&follower_attempts);
    let follower_path = path.clone();
    let follower = std::thread::spawn(move || {
        let result = open_state_db_with_retry(&follower_path, policy, OpenIntent::Required, || {
            observed_attempts.fetch_add(1, Ordering::Relaxed);
        });
        matches!(result, Err(redb::DatabaseError::DatabaseAlreadyOpen))
    });

    leader.join().expect("leader thread");
    assert!(follower.join().expect("follower thread"));
    assert_eq!(
        follower_attempts.load(Ordering::Relaxed),
        0,
        "a queued opener must re-check the breaker after acquiring the mutex"
    );
}

// Issue #2230: a fully contended open must make O(log) attempts, not
// O(budget / fixed_delay). With the old fixed 10 ms sleep a 1 s budget was
// ~100 attempts; exponential backoff from 10 ms makes it ~7.
#[test]
fn contended_open_backs_off_exponentially() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);

    let budget = Duration::from_secs(1);
    let policy = RetryPolicy {
        timeout: budget,
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(400),
        breaker_threshold: 0,
        breaker_cooloff: Duration::ZERO,
        summary_interval: Duration::ZERO,
    };
    let started = Instant::now();
    let result = open_state_db_with_retry(&path, policy, OpenIntent::Required, || {
        observed.fetch_add(1, Ordering::Relaxed);
    });

    assert!(result.is_err(), "a held database must not open");
    let attempts = attempts.load(Ordering::Relaxed);
    // The budget must still be honored - backoff trades attempt count for
    // sleep length, it does not shorten the wait.
    assert!(
        started.elapsed() >= budget,
        "backoff must not cut the budget short, took {:?}",
        started.elapsed()
    );
    assert!(
        attempts >= 3,
        "backoff must still retry a handful of times, got {attempts}"
    );
    assert!(
        attempts <= 15,
        "a {budget:?} budget must cost O(log) attempts, not O(500); got {attempts} \
         (issue #2230)"
    );
}

// Issue #2230: consecutive exhaustions on one path must trip a breaker so
// the next caller fails fast instead of paying the budget again, and the
// breaker must reopen the path once the cooloff elapses.
#[test]
fn breaker_opens_after_repeated_exhaustion_then_closes() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");

    let budget = Duration::from_millis(300);
    let cooloff = Duration::from_millis(400);
    let policy = RetryPolicy {
        timeout: budget,
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(50),
        breaker_threshold: 2,
        breaker_cooloff: cooloff,
        summary_interval: Duration::from_secs(60),
    };

    for attempt in 0..2 {
        let started = Instant::now();
        let result = open_state_db_with_retry(&path, policy, OpenIntent::Required, || {});
        assert!(result.is_err(), "attempt {attempt} must fail");
        assert!(
            started.elapsed() >= budget,
            "attempt {attempt} must pay the full budget before the breaker trips"
        );
    }

    // Breaker is now open: this must return without waiting.
    let started = Instant::now();
    let result = open_state_db_with_retry(&path, policy, OpenIntent::Required, || {
        panic!("an open breaker must not enter the retry loop");
    });
    let fast_fail = started.elapsed();
    assert!(matches!(
        result,
        Err(redb::DatabaseError::DatabaseAlreadyOpen)
    ));
    assert!(
        fast_fail < budget / 3,
        "an open breaker must fail fast, took {fast_fail:?} against a {budget:?} budget"
    );

    // ...and after the cooloff the breaker closes and the budget is paid
    // again.
    std::thread::sleep(cooloff + Duration::from_millis(50));
    let started = Instant::now();
    let result = open_state_db_with_retry(&path, policy, OpenIntent::Required, || {});
    assert!(result.is_err(), "the blocker is still holding the database");
    assert!(
        started.elapsed() >= budget,
        "a closed breaker must retry for real again, took {:?}",
        started.elapsed()
    );

    let log = fs::read_to_string(dir.path().join("logs").join(CONTENTION_LOG_FILE))
        .expect("contention log");
    assert_eq!(
        log.matches(r#""event":"state_db_open_breaker_opened""#)
            .count(),
        1,
        "exactly one breaker-open lifecycle event, not one per suppressed open:\n{log}"
    );
    assert_eq!(
        log.matches(r#""event":"state_db_open_breaker_closed""#)
            .count(),
        1,
        "exactly one breaker-close lifecycle event:\n{log}"
    );
}

// Issue #2230: the field evidence was 1,347 near-identical exhaustion
// records. Inside the summary window the failures are counted, not
// written, and the next emitted record carries the roll-up.
#[test]
fn repeated_exhaustions_are_aggregated_not_one_line_each() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");
    let _blocker = Database::builder().create(&path).expect("blocking open");

    let policy = RetryPolicy {
        timeout: Duration::from_millis(60),
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
        breaker_threshold: 0,
        breaker_cooloff: Duration::ZERO,
        summary_interval: Duration::from_secs(60),
    };
    for _ in 0..5 {
        assert!(open_state_db_with_retry(&path, policy, OpenIntent::Required, || {}).is_err());
    }

    let log = fs::read_to_string(dir.path().join("logs").join(CONTENTION_LOG_FILE))
        .expect("contention log");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "five exhaustions inside one summary window must not write five \
         records (issue #2230):\n{log}"
    );

    // The suppressed failures surface in the *next* emitted record.
    let short = RetryPolicy {
        summary_interval: Duration::ZERO,
        ..policy
    };
    assert!(open_state_db_with_retry(&path, short, OpenIntent::Required, || {}).is_err());
    let log = fs::read_to_string(dir.path().join("logs").join(CONTENTION_LOG_FILE))
        .expect("contention log");
    let last = log.lines().last().expect("a second record");
    assert!(last.contains(r#""suppressed":4"#), "{last}");
    assert!(last.contains(r#""worst_attempts":"#), "{last}");
    assert!(last.contains(r#""worst_elapsed_ms":"#), "{last}");
}

// A clean open must not write a contention record - otherwise the log
// becomes noise and stops being evidence of a real problem.
#[test]
fn uncontended_open_writes_no_forensic_record() {
    let _test_guard = serial_test_guard();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.redb");

    let handle = open_state_db(&path).expect("uncontended open");
    drop(handle);

    assert!(
        !dir.path().join("logs").join(CONTENTION_LOG_FILE).exists(),
        "an uncontended open must leave no contention record"
    );
}
