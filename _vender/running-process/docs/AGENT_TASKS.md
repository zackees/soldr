# Agent Tasks

This file is the active backlog for agent-driven work. Root-level scratch task files should not carry open items.

## Active Backlog

### Runtime Tiny-PDB Validation

- Add a Windows-native debugger or symbolizer path that reliably consumes the shipped tiny PDB during live stack-dump tests.
- Make the strict path used by `bash ./test --no-skip` pass on supported Windows environments instead of failing or skipping because the local debugger cannot resolve PDB-backed frames.
- Keep static `llvm-pdbutil` verification as the baseline release gate until the runtime debugger path is trustworthy.

### NativeProcess Migration

Design reference:
- [REFACTOR_NATIVE_PROCESS.md](C:/Users/niteris/dev/running-process/REFACTOR_NATIVE_PROCESS.md)

Remaining phases:
- Phase 4: move output buffering, history, and checkpoints into `NativeProcess`
- Phase 5: move `wait_for` orchestration into `NativeProcess`
- Phase 6: move `expect` lifecycle and EOF handling deeper into native code
- Phase 7: move idle detection into `NativeProcess`
- Phase 8: simplify Python `RunningProcess` into a thin facade

Execution notes:
- Rebuild with `./.venv/Scripts/python.exe build.py` before trusting Python test results.
- Run targeted PTY and subprocess tests after each phase.
- Keep phases small and coherent; do not start a new phase while the current one is unstable.

## Archived Root Task Files

The following root files are retained only as redirects or historical breadcrumbs:
- `TASK.md`
- `TODO.md`
- `PLAN_TINY_PDB.md`
