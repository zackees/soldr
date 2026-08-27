# Contributing tests

Soldr separates portable logic tests from contracts that require a real host
operating system. This keeps the Linux development loop fast without replacing
filesystem, process, or platform behavior with mocks that cannot prove the
assumption in question.

## Portable logic first

Keep platform-independent parsing, planning, and policy logic in ordinary unit
or integration tests that run in the Linux workspace suite. Host-specific code
should expose fixture- or environment-driven seams where its decision logic can
be tested without the host facility.

When the behavior itself belongs to the operating system, use a native
behavioral integration test instead. Examples include filesystem attributes,
path handling, mapped executables, reparse points, and process lifetime rules.
Put each contract in a dedicated **module**, prefer adding it to an existing
test target under `crates/<crate>/tests/` rather than creating a new top-level
file, gate the module (`#[cfg(windows)]`, for example — or the whole binary
with `#![cfg(windows)]` when the target is host-specific end to end), and
explain the assumption and its failure mode in module documentation. Every
top-level file in `tests/` is compiled as its own executable statically
linking the full soldr graph; that per-file fan-out is how the suite reached
~110 linked binaries and a 3.3 GB CI archive (soldr#2931). Reserve a new test
target for a genuinely new domain; soldr#2934 tracks the module-based
category layout. Use deterministic fixtures where
possible and print an explicit skip reason when a test genuinely depends on
host-installed tooling.

`crates/soldr-cli/tests/windows_delete_semantics.rs` is the reference example:
it pins Windows deletion behavior that had previously been inferred from a
different API. Executable Rust tests are plain `#[test]` functions; per-test
timeouts come from cargo-nextest (`.config/nextest.toml`), and the workspace
guard in `crates/soldr-cli/tests/no_timed_test_guard.rs` keeps the removed
`timed_test!` watchdog from returning (soldr#2493).

On Unix hosts, Nextest runs each test through
`.github/scripts/nextest_timeout_wrapper.py`. When Nextest's per-test timeout
sends SIGTERM, the wrapper terminates the isolated child process group and
drains stdout and stderr to EOF before returning. On Linux it first dumps the
child thread stacks (or `/proc` thread state when a debugger is unavailable).
The configured 30-second Nextest grace period bounds that diagnostic shutdown.
Other Unix hosts still get termination and output draining but currently lack
a thread dumper. Windows Nextest timeouts kill their job object immediately,
so the graceful hook cannot run there; output captured before termination is
still retained by Nextest.

## How native tests reach CI

Native behavioral tests do not run in the Ubuntu lint job. The existing target
lanes carry them through this path:

1. `.github/workflows/_ci-cross-build-linux.yml` creates a target-specific,
   complete `--workspace` nextest archive.
2. `.github/workflows/_ci-target-run.yml` replays that archive without test or
   package filters on a native runner.
3. `.github/workflows/ci.yml` connects the producer and consumer. Windows uses
   the existing `windows-2025` x64 and `windows-11-arm` ARM64 runners.

A target only gets step 2 when the replay lands on a runner that is **native
to that target**. `x86_64-unknown-linux-gnu` and `x86_64-unknown-linux-musl`
build on `ubuntu-24.04` and have no other runner to reach, so they are
build-only lanes (`kind: cross-build` in `ci/canonical-targets.json`) — a
replay there would re-run the suite on the image it was built on, which is the
degenerate split soldr#1978 item 3 removed. Their artifact-level invariants are
checked in the build lane instead (`verify_static_link.py`,
`verify_glibc_baseline.py`). Every cross-arch target keeps its target-run.

Linux-runnable contract tests in `tests/test_cross_compile_workflows.py` protect
that route from silently dropping native tests. Add a new runner only when the
behavior cannot be represented by an archived integration test; do not add a
native lint duplicate merely because the main lint job uses Linux.

## Review checklist

When a change relies on host behavior:

- state the assumption in a native behavioral test, not only in prose or a
  shell-specific reproduction;
- keep portable decisions covered in the Linux suite;
- gate the complete integration-test binary for the host it requires;
- run the suite with `soldr cargo nextest run` and avoid unbounded
  environmental waits;
- preserve complete target-run coverage — the union of replay partitions must
  still execute every test — within the archive's explicit byte/disk budget
  (soldr#2931: linked test products are ephemeral transport, never cached, and
  the bundle must stay compact and single-extraction); and
- validate host failures by test name, comparing known runner flakes against
  `main` before changing a nextest budget.
