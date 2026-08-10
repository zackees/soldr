#![cfg(feature = "client")]

use running_process::broker::server::metrics::{MetricDescriptor, MetricKind, BROKER_METRICS};

const EXPECTED_METRICS: &[MetricDescriptor] = &[
    MetricDescriptor {
        name: "running_process_broker_v1_hello_total",
        kind: MetricKind::Counter,
        labels: &["service", "version", "outcome"],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_hello_duration_seconds",
        kind: MetricKind::Histogram,
        labels: &["service"],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_active_backends",
        kind: MetricKind::Gauge,
        labels: &["service"],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_spawn_attempts_total",
        kind: MetricKind::Counter,
        labels: &["service", "version", "outcome"],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_spawn_budget_remaining",
        kind: MetricKind::Gauge,
        labels: &["service", "version"],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_connections_open",
        kind: MetricKind::Gauge,
        labels: &[],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_fd_usage_ratio",
        kind: MetricKind::Gauge,
        labels: &[],
    },
    MetricDescriptor {
        name: "running_process_broker_v1_uptime_seconds",
        kind: MetricKind::Gauge,
        labels: &[],
    },
];

#[test]
fn broker_metric_names_are_frozen() {
    assert_eq!(BROKER_METRICS, EXPECTED_METRICS);
}

#[test]
fn broker_metric_names_keep_v1_prefix() {
    for metric in BROKER_METRICS {
        assert!(
            metric.name.starts_with("running_process_broker_v1_"),
            "metric without v1 prefix: {}",
            metric.name
        );
    }
}
