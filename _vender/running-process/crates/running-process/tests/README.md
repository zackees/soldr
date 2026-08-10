# `running-process` integration tests

Integration tests for the `running-process` crate. Each `*.rs` file at the top
level is a separate cargo test binary (no `#[cfg(test)]` module needed); files
under sub-directories are shared helpers.

## Tests by subsystem

### Broker v1 / v2

- `broker_v2_scaffold_accepts_connection.rs` — end-to-end test of the
  `running-process-broker-v2` binary. Spawns the broker (with a temp
  service-def dir + stub servicedef), connects as a client, asserts the
  Hello round-trip succeeds.
- `brokered_backend_ui.rs` — trybuild UI test for the `BrokeredBackend` trait.
- `broker/` — shared broker test helpers.

### Daemon / cross-process

Test files like `daemon_autostart_test.rs`, `daemon_backlog_accumulation_test.rs`,
`daemon_cross_process_pty_attach_test.rs`, `daemon_fast_ctrl_c_handoff_test.rs`,
`containment_test.rs`, etc. exercise the live daemon binary end-to-end.

### Cleanup / common

- `cleanup/` — registry GC scenarios.
- `common/` — shared test fixtures (`tempfile`-rooted dirs, in-memory backends).

## Test naming + scope

- One file per scenario family.
- Use `#![cfg(feature = "...")]` at the top when the test depends on a
  feature-gated subsystem (`client`, `daemon`, etc.).
- `tempfile::tempdir()` for any disk I/O — never write into `target/` or
  the user's real cache dirs.
- For tests that spawn the broker binary, set
  `RUNNING_PROCESS_SERVICE_DEF_DIR` to the per-test tempdir +
  install whatever stub servicedef the broker's loader needs. Otherwise
  the loader's `ErrorServiceUnknown` path fires and the Hello returns
  Refused.
