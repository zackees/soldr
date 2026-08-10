# Rust Migration Plan

## Goal

Replace the Python subprocess implementation with a Rust core so stdout and stderr can be drained independently without the Windows pipe behavior that forced merged output.

## Plan

1. Freeze externally visible Python behavior with tests.
2. Introduce a Rust workspace and native extension build path.
3. Move process spawning, waiting, and stream draining into Rust.
4. Keep a small Python compatibility layer for the public API.
5. Add tests for the new split-stream contract and compatibility helpers.

## Delivered

- Rust workspace with `running-process` and `running-process-py`
- PyO3-backed `running_process._native`
- Thin Python wrapper in `src/running_process`
- Separate stdout/stderr history, draining, and readiness checks
- Combined stream retained for compatibility
- Repo scripts for install, lint, test, and publish
- Rust tests plus Python integration tests

## Current Scope

The migration is focused on the process runtime and the split-stream API. PTY support is currently exposed as unavailable in the Python layer rather than reimplemented on the Rust side.

## Validation

- `./install`
- `soldr cargo test --workspace`
- `uv run --module ci.lint`
- `uv run --module ci.test`
