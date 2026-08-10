#![cfg(feature = "client")]

use std::time::Duration;

use running_process::broker::server::handoff::{
    collect_latency_samples, compare_handoff_latency, summarize_latency_samples,
    HandoffLatencyError,
};

fn micros(value: u64) -> Duration {
    Duration::from_micros(value)
}

#[test]
fn collect_latency_samples_discards_warmup_and_keeps_measured_iterations() {
    let mut calls = 0_u64;
    let samples = collect_latency_samples(3, 5, || {
        calls += 1;
        micros(calls)
    });

    assert_eq!(calls, 8, "warmup iterations must still run the operation");
    assert_eq!(
        samples,
        vec![micros(4), micros(5), micros(6), micros(7), micros(8)],
        "warmup samples must be discarded, measured samples kept in order"
    );
}

#[test]
fn collect_latency_samples_with_zero_iterations_collects_nothing() {
    let mut calls = 0_usize;
    let samples = collect_latency_samples(2, 0, || {
        calls += 1;
        micros(1)
    });

    assert_eq!(calls, 2);
    assert!(samples.is_empty());
}

#[test]
fn summarize_latency_samples_reports_ordered_percentiles_or_nothing() {
    assert_eq!(summarize_latency_samples(&[]), None);

    let summary = summarize_latency_samples(&[micros(30), micros(10), micros(20)]).unwrap();
    assert_eq!(summary.sample_count, 3);
    assert_eq!(summary.p50, micros(20));
    assert_eq!(summary.p99, micros(30));
    assert!(summary.p50 <= summary.p99);
}

#[test]
fn measured_handoff_samples_must_beat_reconnect_fallback_at_p50_and_p99() {
    let handoff = [
        micros(8),
        micros(9),
        micros(9),
        micros(10),
        micros(10),
        micros(11),
        micros(11),
        micros(12),
        micros(12),
        micros(13),
    ];
    let fallback = [
        micros(23),
        micros(24),
        micros(24),
        micros(25),
        micros(25),
        micros(26),
        micros(27),
        micros(28),
        micros(29),
        micros(30),
    ];

    let comparison = compare_handoff_latency(&handoff, &fallback).unwrap();

    assert!(comparison.proves_handoff_faster());
    assert_eq!(comparison.handoff.sample_count, handoff.len());
    assert_eq!(comparison.fallback.sample_count, fallback.len());
    assert!(comparison.p50_savings() >= micros(15));
    assert!(comparison.p99_savings() >= micros(17));
}

#[test]
fn equal_or_slower_handoff_samples_do_not_pass_the_phase6_latency_proof() {
    let fallback = [micros(10), micros(11), micros(12), micros(13)];

    assert_eq!(
        compare_handoff_latency(&[micros(10), micros(11), micros(12), micros(13)], &fallback),
        Err(HandoffLatencyError::P50NotFaster {
            handoff: micros(11),
            fallback: micros(11),
        })
    );
    assert_eq!(
        compare_handoff_latency(&[micros(9), micros(10), micros(20), micros(21)], &fallback),
        Err(HandoffLatencyError::P99NotFaster {
            handoff: micros(21),
            fallback: micros(13),
        })
    );
}

#[test]
fn latency_comparison_requires_both_handoff_and_fallback_samples() {
    assert_eq!(
        compare_handoff_latency(&[], &[micros(1)]),
        Err(HandoffLatencyError::EmptyHandoffSamples)
    );
    assert_eq!(
        compare_handoff_latency(&[micros(1)], &[]),
        Err(HandoffLatencyError::EmptyFallbackSamples)
    );
}
