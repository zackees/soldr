# rust_plan_tests

Unit tests for [`crate::rust_plan`], split across topic files so no single
source file blows past the project's 1,000-LOC ceiling. Wired in via
`#[path = "rust_plan_tests/mod.rs"] mod tests;` at the bottom of
`rust_plan.rs`.

- `plan_build.rs` — `build_rust_artifact_plan` + allowed/dropped class metadata.
- `local_roundtrip.rs` — in-process local save/restore through
  `run_zccache_rust_plan` (soldr#1368: no `zccache rust-plan` subprocess).
- `warm_restore.rs` / `prepopulated_target.rs` — warm-restore short-circuit.
- `manifest.rs` / `bundle_walk.rs` — thin-slice manifest emission.
- `orphan_rmeta.rs` — orphan-rmeta pruning after a failed build.
- `restore_gc_protection.rs` — issue #1558: pre-cargo target GC must not
  prune hash families a verified rust-plan restore just materialized.
- `wire_compat.rs` — plan protobuf encode/decode compatibility.
