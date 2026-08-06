# Contributing tests

Soldr separates portable logic tests from contracts that require a real host
operating system. This keeps the Linux development loop fast without replacing
filesystem, process, or platform behavior with mocks that cannot prove the
assumption in question.

## Fresh checkout prerequisite

The repository embeds zccache as a required submodule. After cloning or
creating a fresh worktree, run this before any Soldr build command:

```bash
git submodule update --init _vender/zccache
```

Soldr will report this exact remedy if it detects the missing submodule. It
never initializes it itself, so a build does not unexpectedly fetch from the
network.

## Portable logic first

Keep platform-independent parsing, planning, and policy logic in ordinary unit
or integration tests that run in the Linux workspace suite. Host-specific code
should expose fixture- or environment-driven seams where its decision logic can
be tested without the host facility.

When the behavior itself belongs to the operating system, use a native
behavioral integration test instead. Examples include filesystem attributes,
path handling, mapped executables, reparse points, and process lifetime rules.
Put each contract in a dedicated file under `crates/<crate>/tests/`, gate the
whole test binary (`#![cfg(windows)]`, for example), and explain the assumption
and its failure mode in module documentation. Use deterministic fixtures where
possible and print an explicit skip reason when a test genuinely depends on
host-installed tooling.

`crates/soldr-cli/tests/windows_delete_semantics.rs` is the reference example:
it pins Windows deletion behavior that had previously been inferred from a
different API. All executable Rust tests must use `timed_test!`; the workspace
guard in `crates/soldr-cli/tests/timed_test_lint.rs` enforces that rule.

## How native tests reach CI

Native behavioral tests do not run in the Ubuntu lint job. The existing target
lanes carry them through this path:

1. `.github/workflows/_ci-cross-build-linux.yml` creates a target-specific,
   complete `--workspace` nextest archive.
2. `.github/workflows/_ci-target-run.yml` replays that archive without test or
   package filters on a native runner.
3. `.github/workflows/ci.yml` connects the producer and consumer. Windows uses
   the existing `windows-2025` x64 and `windows-11-arm` ARM64 runners.

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
- use `timed_test!` and avoid unbounded environmental waits;
- preserve the complete, unfiltered archive and target-run path; and
- validate host failures by test name, comparing known runner flakes against
  `main` before changing a watchdog budget.
