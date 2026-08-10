#![cfg(feature = "client")]

use std::time::Duration;

use running_process::broker::server::{
    SpawnWaitDecision, SpawnWaitPolicy, SpawnWaitProbe, DEFAULT_SPAWN_WAIT_HARD_CEILING,
    SPAWN_WAIT_BACKOFF_SEQUENCE,
};

fn probe(
    elapsed: Duration,
    daemon_alive: bool,
    endpoint_ready: bool,
    attempt: usize,
) -> SpawnWaitProbe {
    SpawnWaitProbe::new(elapsed, daemon_alive, endpoint_ready, attempt)
}

#[test]
fn default_hard_ceiling_is_sixty_seconds() {
    let policy = SpawnWaitPolicy::new();

    assert_eq!(DEFAULT_SPAWN_WAIT_HARD_CEILING, Duration::from_secs(60));
    assert_eq!(policy.hard_ceiling(), Duration::from_secs(60));
}

#[test]
fn timeout_is_hard_bounded_not_infinite() {
    let policy = SpawnWaitPolicy::with_hard_ceiling(Duration::from_millis(125));

    assert_eq!(
        policy.decide(probe(
            Duration::from_millis(100),
            true,
            false,
            SPAWN_WAIT_BACKOFF_SEQUENCE.len(),
        )),
        SpawnWaitDecision::Sleep {
            duration: Duration::from_millis(25),
        }
    );

    assert_eq!(
        policy.decide(probe(Duration::from_millis(125), true, false, 0)),
        SpawnWaitDecision::Timeout {
            hard_ceiling: Duration::from_millis(125),
        }
    );
}

#[test]
fn dead_process_returns_daemon_exited_before_ready() {
    let policy = SpawnWaitPolicy::new();

    assert_eq!(
        policy.decide(probe(Duration::from_millis(25), false, false, 0)),
        SpawnWaitDecision::DaemonExitedBeforeReady
    );
}

#[test]
fn endpoint_ready_wins() {
    let policy = SpawnWaitPolicy::with_hard_ceiling(Duration::from_millis(1));

    assert_eq!(
        policy.decide(probe(Duration::from_secs(30), false, true, 5)),
        SpawnWaitDecision::EndpointReady
    );
}

#[test]
fn backoff_sequence_and_cap_match_spawn_wait_contract() {
    let policy = SpawnWaitPolicy::new();

    assert_eq!(
        SPAWN_WAIT_BACKOFF_SEQUENCE,
        [
            Duration::from_millis(50),
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
        ]
    );

    for (attempt, expected) in SPAWN_WAIT_BACKOFF_SEQUENCE.iter().copied().enumerate() {
        assert_eq!(policy.backoff_for_attempt(attempt), expected);
    }

    assert_eq!(
        policy.backoff_for_attempt(SPAWN_WAIT_BACKOFF_SEQUENCE.len()),
        Duration::from_secs(2)
    );
    assert_eq!(policy.backoff_for_attempt(999), Duration::from_secs(2));
}
