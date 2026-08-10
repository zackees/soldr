# tests/

Python (pytest) test suite for the `running_process` package and the CI
tooling under `ci/`. Run via `./test` or
`uv run --no-sync pytest tests -v`; integration ("live") tests need
`RUNNING_PROCESS_LIVE_TESTS=1`. Per-test 2-minute wall-clock kill via
pytest-timeout (see repo `CLAUDE.md` → "Per-test deadlock guard").

Layout:

- `test_ci_*.py` — coverage for the `ci/` module scripts
- `test_pty_*.py` + `pty/` — PTY behavior (ConPTY on Windows, portable-pty on Unix)
- `encoding/` — Windows console encoding suite (opt-in via `RUNNING_PROCESS_WINDOWS_ENCODING_TESTS=1`)
- `test_daemon_*.py` — daemon spawn, env, PID tracking, trampoline
- `conftest.py`, `process_helpers.py`, `pid_tracker.py` — shared fixtures/helpers
