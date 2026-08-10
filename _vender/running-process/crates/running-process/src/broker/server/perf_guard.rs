//! Hello latency budget enforcement for the v1 broker.

use std::time::Duration;

/// Minimum sample count for the CI Hello perf guard.
pub const HELLO_PERF_SAMPLE_COUNT: usize = 10_000;

/// Environment variable that enables the real local-socket Hello perf gate.
pub const HELLO_PERF_GUARD_ENV: &str = "RUNNING_PROCESS_BROKER_HELLO_PERF_GUARD";

/// Frozen Hello P50 latency budget.
pub const HELLO_P50_BUDGET: Duration = Duration::from_micros(200);

/// Frozen Hello P99 latency budget.
pub const HELLO_P99_BUDGET: Duration = Duration::from_millis(1);

/// Percentile summary for one Hello latency run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloLatencySummary {
    /// Number of samples summarized.
    pub sample_count: usize,
    /// P50 latency.
    pub p50: Duration,
    /// P99 latency.
    pub p99: Duration,
}

/// Errors returned when a perf run violates the v1 budget.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PerfGuardError {
    /// No samples were supplied.
    #[error("hello perf guard received no samples")]
    EmptySamples,
    /// The sample set is too small for CI gating.
    #[error("hello perf guard needs at least {required} samples, got {actual}")]
    TooFewSamples {
        /// Required sample count.
        required: usize,
        /// Actual sample count.
        actual: usize,
    },
    /// P50 exceeded the frozen budget.
    #[error("hello P50 budget exceeded: actual {actual:?}, budget {budget:?}")]
    P50Exceeded {
        /// Actual percentile.
        actual: Duration,
        /// Frozen budget.
        budget: Duration,
    },
    /// P99 exceeded the frozen budget.
    #[error("hello P99 budget exceeded: actual {actual:?}, budget {budget:?}")]
    P99Exceeded {
        /// Actual percentile.
        actual: Duration,
        /// Frozen budget.
        budget: Duration,
    },
}

/// Summarize one non-empty latency sample set.
pub fn summarize_hello_latencies(
    samples: &[Duration],
) -> Result<HelloLatencySummary, PerfGuardError> {
    if samples.is_empty() {
        return Err(PerfGuardError::EmptySamples);
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Ok(HelloLatencySummary {
        sample_count: sorted.len(),
        p50: percentile_nearest_rank(&sorted, 50),
        p99: percentile_nearest_rank(&sorted, 99),
    })
}

/// Enforce the frozen v1 Hello latency budget.
pub fn enforce_hello_latency_budget(
    samples: &[Duration],
) -> Result<HelloLatencySummary, PerfGuardError> {
    let summary = summarize_hello_latencies(samples)?;
    if summary.sample_count < HELLO_PERF_SAMPLE_COUNT {
        return Err(PerfGuardError::TooFewSamples {
            required: HELLO_PERF_SAMPLE_COUNT,
            actual: summary.sample_count,
        });
    }
    if summary.p50 > HELLO_P50_BUDGET {
        return Err(PerfGuardError::P50Exceeded {
            actual: summary.p50,
            budget: HELLO_P50_BUDGET,
        });
    }
    if summary.p99 > HELLO_P99_BUDGET {
        return Err(PerfGuardError::P99Exceeded {
            actual: summary.p99,
            budget: HELLO_P99_BUDGET,
        });
    }
    Ok(summary)
}

fn percentile_nearest_rank(sorted: &[Duration], percentile: usize) -> Duration {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentile));

    let rank = sorted.len() * percentile;
    let index = rank.div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}
