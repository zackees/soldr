//! Frozen OpenMetrics names for the v1 broker.

/// Prefix for every v1 broker metric.
pub const METRIC_PREFIX: &str = "running_process_broker_v1_";

/// Metric type used by the OpenMetrics renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonic counter.
    Counter,
    /// Gauge value.
    Gauge,
    /// Histogram series.
    Histogram,
}

/// One frozen metric descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricDescriptor {
    /// OpenMetrics name.
    pub name: &'static str,
    /// Metric type.
    pub kind: MetricKind,
    /// Label names in stable render order.
    pub labels: &'static [&'static str],
}

const SERVICE_VERSION_OUTCOME: &[&str] = &["service", "version", "outcome"];
const SERVICE_VERSION: &[&str] = &["service", "version"];
const SERVICE: &[&str] = &["service"];
const NO_LABELS: &[&str] = &[];

/// Hello requests by service, version, and outcome.
pub const HELLO_TOTAL: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_hello_total",
    kind: MetricKind::Counter,
    labels: SERVICE_VERSION_OUTCOME,
};

/// Hello latency histogram by service.
pub const HELLO_DURATION_SECONDS: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_hello_duration_seconds",
    kind: MetricKind::Histogram,
    labels: SERVICE,
};

/// Live backend count by service.
pub const ACTIVE_BACKENDS: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_active_backends",
    kind: MetricKind::Gauge,
    labels: SERVICE,
};

/// Backend spawn attempts by service, version, and outcome.
pub const SPAWN_ATTEMPTS_TOTAL: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_spawn_attempts_total",
    kind: MetricKind::Counter,
    labels: SERVICE_VERSION_OUTCOME,
};

/// Remaining spawn budget by service and version.
pub const SPAWN_BUDGET_REMAINING: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_spawn_budget_remaining",
    kind: MetricKind::Gauge,
    labels: SERVICE_VERSION,
};

/// Open broker control-plane connections.
pub const CONNECTIONS_OPEN: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_connections_open",
    kind: MetricKind::Gauge,
    labels: NO_LABELS,
};

/// Process file-descriptor or handle pressure ratio.
pub const FD_USAGE_RATIO: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_fd_usage_ratio",
    kind: MetricKind::Gauge,
    labels: NO_LABELS,
};

/// Broker process uptime in seconds.
pub const UPTIME_SECONDS: MetricDescriptor = MetricDescriptor {
    name: "running_process_broker_v1_uptime_seconds",
    kind: MetricKind::Gauge,
    labels: NO_LABELS,
};

/// Complete frozen metric set.
pub const BROKER_METRICS: &[MetricDescriptor] = &[
    HELLO_TOTAL,
    HELLO_DURATION_SECONDS,
    ACTIVE_BACKENDS,
    SPAWN_ATTEMPTS_TOTAL,
    SPAWN_BUDGET_REMAINING,
    CONNECTIONS_OPEN,
    FD_USAGE_RATIO,
    UPTIME_SECONDS,
];
