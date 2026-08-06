# Fetch network boundary

All production HTTP traffic for downloads and control-plane metadata lives in
`crates/soldr-fetch/src/fetch/stream_download.rs`. Callers choose either a
bounded control request or a streamed asset request; they do not construct
reqwest clients, send requests, or consume response bodies directly.

Control requests use distinct connection, header, and small-body deadlines.
Asset downloads use a connection/header deadline, a progress-resetting idle
watchdog, and a six-hour global safety ceiling. They stream directly into a
temporary file while incrementally computing SHA-256; callers only receive a
completed temporary file after the stream succeeds. A failed asset attempt is
therefore retried from a clean temporary file, never resumed from unverified
partial bytes.

`dylints/ban_raw_network_access` enforces this boundary for production
`soldr-fetch` Rust. The dependency guard additionally keeps `reqwest` owned by
`soldr-fetch`, preventing the CLI facade or other internal crates from adding a
parallel network path.
