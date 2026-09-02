//! Broker route-acquisition deadlines and their `soldr doctor` surface.
//!
//! Split out of `broker_server.rs` (soldr#2493) to keep that file under the
//! workspace's 1,000-line production-source ceiling — this cluster has no
//! access to the broker's private connection-handling state, so it moves
//! cleanly.

use std::time::Duration;

const DEFAULT_FIRST_RESPONSE_MS: u64 = 2_000;
const DEFAULT_PROGRESS_SILENCE_MS: u64 = 5_000;
const DEFAULT_ROUTE_CEILING_MS: u64 = 120_000;
const DEFAULT_BUSY_BUDGET_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BrokerDeadlines {
    pub(crate) busy_budget: Duration,
    pub(crate) first_response: Duration,
    pub(crate) progress_silence: Duration,
    pub(crate) route_ceiling: Duration,
}

impl BrokerDeadlines {
    pub(crate) fn from_env() -> Self {
        Self {
            busy_budget: env_duration("SOLDR_BROKER_BUSY_BUDGET_MS", DEFAULT_BUSY_BUDGET_MS),
            first_response: env_duration(
                "SOLDR_BROKER_FIRST_RESPONSE_MS",
                DEFAULT_FIRST_RESPONSE_MS,
            ),
            progress_silence: env_duration(
                "SOLDR_BROKER_PROGRESS_SILENCE_MS",
                DEFAULT_PROGRESS_SILENCE_MS,
            ),
            route_ceiling: env_duration("SOLDR_ROUTE_ACQUIRE_CEILING_MS", DEFAULT_ROUTE_CEILING_MS),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DoctorBrokerDeadline {
    pub(crate) name: &'static str,
    pub(crate) env_var: &'static str,
    pub(crate) default_ms: u64,
    pub(crate) effective_ms: u64,
    pub(crate) source: &'static str,
}

pub(crate) fn doctor_deadlines() -> Vec<DoctorBrokerDeadline> {
    let effective = BrokerDeadlines::from_env();
    [
        (
            "broker busy retry",
            "SOLDR_BROKER_BUSY_BUDGET_MS",
            DEFAULT_BUSY_BUDGET_MS,
            effective.busy_budget,
        ),
        (
            "broker first response",
            "SOLDR_BROKER_FIRST_RESPONSE_MS",
            DEFAULT_FIRST_RESPONSE_MS,
            effective.first_response,
        ),
        (
            "broker progress silence",
            "SOLDR_BROKER_PROGRESS_SILENCE_MS",
            DEFAULT_PROGRESS_SILENCE_MS,
            effective.progress_silence,
        ),
        (
            "broker route ceiling",
            "SOLDR_ROUTE_ACQUIRE_CEILING_MS",
            DEFAULT_ROUTE_CEILING_MS,
            effective.route_ceiling,
        ),
    ]
    .into_iter()
    .map(
        |(name, env_var, default_ms, duration)| DoctorBrokerDeadline {
            name,
            env_var,
            default_ms,
            effective_ms: duration.as_millis() as u64,
            source: match std::env::var(env_var) {
                Ok(value) if value.trim().parse::<u64>().is_ok_and(|value| value > 0) => "override",
                Ok(_) => "default (override ignored: expected positive milliseconds)",
                Err(_) => "default",
            },
        },
    )
    .collect()
}

pub(crate) fn print_doctor_deadlines() {
    println!("\nbroker route deadlines:");
    for row in doctor_deadlines() {
        println!(
            "  {:<24} {:>7} ms  [{} via {}]",
            row.name, row.effective_ms, row.source, row.env_var
        );
    }
}

fn env_duration(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default_ms),
    )
}
