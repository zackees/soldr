//! Latency comparison helpers for Phase 6 handoff validation.
//!
//! Handoff is only worth enabling when the platform transfer path is faster
//! than the reconnect fallback it replaces. This module keeps that comparison
//! deterministic and testable; callers can feed real measurements when the
//! end-to-end handoff path is wired into a perf run.

use std::time::Duration;

/// Collect one warmed-up latency sample set.
///
/// `sample` runs once per iteration and returns the duration of the region
/// the caller timed (callers measure with [`std::time::Instant`], the
/// process-wide monotonic clock, so wall-clock adjustments cannot skew the
/// samples). The first `warmup` iterations run but are discarded so cold
/// caches, lazy allocations, and first-connection costs do not distort the
/// measured distribution.
pub fn collect_latency_samples<F>(warmup: usize, iterations: usize, mut sample: F) -> Vec<Duration>
where
    F: FnMut() -> Duration,
{
    for _ in 0..warmup {
        let _ = sample();
    }
    (0..iterations).map(|_| sample()).collect()
}

/// Summarize one measured sample set at the frozen P50/P99 percentiles.
///
/// Returns `None` when no samples were collected, so harnesses cannot
/// report percentiles for an empty run.
pub fn summarize_latency_samples(samples: &[Duration]) -> Option<HandoffLatencySummary> {
    summarize_handoff_latencies(samples, EmptySampleSet::Handoff).ok()
}

/// Percentile summary for one handoff latency sample set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffLatencySummary {
    /// Number of samples summarized.
    pub sample_count: usize,
    /// P50 latency.
    pub p50: Duration,
    /// P99 latency.
    pub p99: Duration,
}

/// Comparison between optimized handoff and reconnect fallback samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffLatencyComparison {
    /// Summary for successful handoff samples.
    pub handoff: HandoffLatencySummary,
    /// Summary for reconnect fallback samples.
    pub fallback: HandoffLatencySummary,
}

impl HandoffLatencyComparison {
    /// Return the P50 latency saved by handoff over reconnect fallback.
    pub fn p50_savings(&self) -> Duration {
        self.fallback.p50.saturating_sub(self.handoff.p50)
    }

    /// Return the P99 latency saved by handoff over reconnect fallback.
    pub fn p99_savings(&self) -> Duration {
        self.fallback.p99.saturating_sub(self.handoff.p99)
    }

    /// Return true when handoff is strictly faster at both frozen percentiles.
    pub fn proves_handoff_faster(&self) -> bool {
        self.handoff.p50 < self.fallback.p50 && self.handoff.p99 < self.fallback.p99
    }
}

/// Errors returned when handoff latency does not beat reconnect fallback.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandoffLatencyError {
    /// No handoff samples were supplied.
    #[error("handoff latency comparison received no handoff samples")]
    EmptyHandoffSamples,
    /// No reconnect fallback samples were supplied.
    #[error("handoff latency comparison received no fallback samples")]
    EmptyFallbackSamples,
    /// Handoff P50 was not faster than reconnect fallback P50.
    #[error(
        "handoff P50 was not faster than fallback: handoff {handoff:?}, fallback {fallback:?}"
    )]
    P50NotFaster {
        /// Handoff P50.
        handoff: Duration,
        /// Reconnect fallback P50.
        fallback: Duration,
    },
    /// Handoff P99 was not faster than reconnect fallback P99.
    #[error(
        "handoff P99 was not faster than fallback: handoff {handoff:?}, fallback {fallback:?}"
    )]
    P99NotFaster {
        /// Handoff P99.
        handoff: Duration,
        /// Reconnect fallback P99.
        fallback: Duration,
    },
}

/// Compare measured handoff samples against reconnect fallback samples.
///
/// The comparison requires handoff to be strictly faster at both P50 and P99.
/// This prevents Phase 6 from treating equal or slower handle passing as a
/// successful optimization.
pub fn compare_handoff_latency(
    handoff_samples: &[Duration],
    fallback_samples: &[Duration],
) -> Result<HandoffLatencyComparison, HandoffLatencyError> {
    let handoff = summarize_handoff_latencies(handoff_samples, EmptySampleSet::Handoff)?;
    let fallback = summarize_handoff_latencies(fallback_samples, EmptySampleSet::Fallback)?;
    let comparison = HandoffLatencyComparison { handoff, fallback };

    if comparison.handoff.p50 >= comparison.fallback.p50 {
        return Err(HandoffLatencyError::P50NotFaster {
            handoff: comparison.handoff.p50,
            fallback: comparison.fallback.p50,
        });
    }
    if comparison.handoff.p99 >= comparison.fallback.p99 {
        return Err(HandoffLatencyError::P99NotFaster {
            handoff: comparison.handoff.p99,
            fallback: comparison.fallback.p99,
        });
    }

    Ok(comparison)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptySampleSet {
    Handoff,
    Fallback,
}

fn summarize_handoff_latencies(
    samples: &[Duration],
    empty: EmptySampleSet,
) -> Result<HandoffLatencySummary, HandoffLatencyError> {
    if samples.is_empty() {
        return Err(match empty {
            EmptySampleSet::Handoff => HandoffLatencyError::EmptyHandoffSamples,
            EmptySampleSet::Fallback => HandoffLatencyError::EmptyFallbackSamples,
        });
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Ok(HandoffLatencySummary {
        sample_count: sorted.len(),
        p50: percentile_nearest_rank(&sorted, 50),
        p99: percentile_nearest_rank(&sorted, 99),
    })
}

fn percentile_nearest_rank(sorted: &[Duration], percentile: usize) -> Duration {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentile));

    let rank = sorted.len() * percentile;
    let index = rank.div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}
