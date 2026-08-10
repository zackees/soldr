# ci/

Python CI entry points and helpers, invoked as `uv run --no-sync --module ci.<name>`.

- `test.py` — Rust (nextest) + Python (pytest) test driver; `--coverage` runs cargo-llvm-cov with a corrupt-profraw prune between run and report; `--live-only` runs integration tests
- `lint.py` — combined Rust + Python lint gate
- `build_wheel.py` / `dev_build.py` — maturin wheel builds (dev/release)
- `publish.py` — manual wheel-collection + crates.io publish fallback (auto-release.yml is the canonical path)
- `version_check.py` — asserts all version strings stay in lockstep
- `coverage-`adjacent: `run_logged.py` (log-teeing wrapper), `render_failure_diagnostics.py`
- `servicedef_proof.py` — platform-default servicedef install-path proof (#386)
- `reproducible.py` — double-build reproducibility spot check (#392)
- `soldr.py` — cargo command indirection (soldr when present, bare cargo otherwise)
- `linux_docker.py` / `dev_docker.py` / `linux_pytest.py` — Linux container harnesses
- `claude_hooks.py` / `codex_hooks.py` / `spawn_path_guard.py` / `check_rust_debug_annotations.py` — agent + repo guards
- `terminal_capability_report.py` — renders the terminal graphics capability matrix artifact
- `env.py` — shared environment helpers
