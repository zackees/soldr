# heavy-leaf-0014

Observability / metrics / tracing subgraph (pure-Rust):
`opentelemetry` (with trace + metrics + logs), `opentelemetry_sdk`
(with rt-tokio + trace + metrics + logs + testing),
`opentelemetry-stdout`, `opentelemetry-semantic-conventions`,
`tracing-opentelemetry`, `tracing-tree`, `tracing-error`,
`tracing-appender`, `metrics`, `metrics-util`,
`metrics-exporter-prometheus`, `prometheus`, `prometheus-client`,
`sentry` (with rustls backend — native-tls avoided). Heavy
proc-macro graph distinct from the prior thirteen leaves'
subgraphs. No `cc-rs` build scripts (native-tls / openssl
avoided). See parent `../README.md` and soldr#648.
