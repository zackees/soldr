//! Adaptive spawn-wait timing model for broker-managed backends.
//!
//! This module deliberately does not touch process handles or sockets. It
//! models the decision loop that a future `wait_for_daemon_ready` implementation
//! will drive with real daemon-liveness and endpoint probes.

use std::time::Duration;

/// Default hard ceiling for waiting until a spawned daemon endpoint is ready.
pub const DEFAULT_SPAWN_WAIT_HARD_CEILING: Duration = Duration::from_secs(60);

/// Adaptive wait sequence used between daemon-ready probes.
pub const SPAWN_WAIT_BACKOFF_SEQUENCE: [Duration; 6] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Policy for deciding one step of the backend daemon ready wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnWaitPolicy {
    hard_ceiling: Duration,
}

impl SpawnWaitPolicy {
    /// Create a policy with the default 60-second hard ceiling.
    pub fn new() -> Self {
        Self::with_hard_ceiling(DEFAULT_SPAWN_WAIT_HARD_CEILING)
    }

    /// Create a policy with an explicit hard ceiling.
    pub fn with_hard_ceiling(hard_ceiling: Duration) -> Self {
        Self { hard_ceiling }
    }

    /// Return the configured hard ceiling.
    pub fn hard_ceiling(&self) -> Duration {
        self.hard_ceiling
    }

    /// Return the adaptive backoff for a zero-based probe attempt.
    ///
    /// Attempts beyond the explicit sequence are capped at the final 2-second
    /// step.
    pub fn backoff_for_attempt(&self, attempt: usize) -> Duration {
        let capped_index = attempt.min(SPAWN_WAIT_BACKOFF_SEQUENCE.len() - 1);
        SPAWN_WAIT_BACKOFF_SEQUENCE[capped_index]
    }

    /// Decide what the wait loop should do after one daemon/endpoint probe.
    pub fn decide(&self, probe: SpawnWaitProbe) -> SpawnWaitDecision {
        if probe.endpoint_ready {
            return SpawnWaitDecision::EndpointReady;
        }

        if !probe.daemon_alive {
            return SpawnWaitDecision::DaemonExitedBeforeReady;
        }

        if probe.elapsed >= self.hard_ceiling {
            return SpawnWaitDecision::Timeout {
                hard_ceiling: self.hard_ceiling,
            };
        }

        SpawnWaitDecision::Sleep {
            duration: self
                .backoff_for_attempt(probe.attempt)
                .min(self.hard_ceiling - probe.elapsed),
        }
    }
}

impl Default for SpawnWaitPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Observed state after one daemon-ready probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnWaitProbe {
    /// Time elapsed since the daemon process was spawned.
    pub elapsed: Duration,
    /// Whether the daemon process is still alive.
    pub daemon_alive: bool,
    /// Whether the daemon endpoint accepted a readiness probe.
    pub endpoint_ready: bool,
    /// Zero-based probe attempt used to select adaptive backoff.
    pub attempt: usize,
}

impl SpawnWaitProbe {
    /// Build a probe observation.
    pub fn new(
        elapsed: Duration,
        daemon_alive: bool,
        endpoint_ready: bool,
        attempt: usize,
    ) -> Self {
        Self {
            elapsed,
            daemon_alive,
            endpoint_ready,
            attempt,
        }
    }
}

/// Decision returned by [`SpawnWaitPolicy`] for one wait-loop step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnWaitDecision {
    /// The endpoint is reachable, so the daemon is ready.
    EndpointReady,
    /// The daemon exited before its endpoint became ready.
    DaemonExitedBeforeReady,
    /// The hard ceiling elapsed before the endpoint became ready.
    Timeout {
        /// Configured hard ceiling that bounded the wait.
        hard_ceiling: Duration,
    },
    /// Sleep for this duration before probing again.
    Sleep {
        /// Capped adaptive backoff duration.
        duration: Duration,
    },
}
