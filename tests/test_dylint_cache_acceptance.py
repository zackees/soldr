from __future__ import annotations

import importlib.util
from pathlib import Path


def load_module():
    path = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    )
    spec = importlib.util.spec_from_file_location("dylint_cache_acceptance", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


dylint_acceptance = load_module()


def test_absolute_watchdog_survives_noisy_semantic_progress() -> None:
    script = dylint_acceptance.BASH
    assert "WATCHDOG_ABSOLUTE_SECS=1800" in script
    assert 'elapsed_secs="$((elapsed_secs + WATCHDOG_POLL_SECS))"' in script

    progress_branch = script.split('if [[ "$semantic_progress" -eq 1 ]]', maxsplit=1)[
        1
    ].split('if [[ "$captured" -eq 1 ]]', maxsplit=1)[0]
    assert "elapsed_secs=0" not in progress_branch
    assert "continue" not in progress_branch

    absolute_branch = script.split(
        'if [[ "$elapsed_secs" -ge "$WATCHDOG_ABSOLUTE_SECS" ]]', maxsplit=1
    )[1].split('elif [[ "$idle_secs"', maxsplit=1)[0]
    assert "absolute_deadline=1" in absolute_branch

    capture_tail = script.split('cat "$dump" >&2', maxsplit=1)[1]
    assert 'if [[ "$absolute_deadline" -eq 1 ]]' in capture_tail
    assert 'terminate_scope "$command_pid"' in capture_tail


def test_initial_dylint_bootstrap_is_inside_monitored_cold_case() -> None:
    script = dylint_acceptance.BASH
    assert "cargo dylint --version" not in script
    assert script.count('"$SOLDR" cargo dylint --all') == 1
    assert "> >(" not in script
    assert '2>&1 | tee -a "$live_log"' in script
    assert 'run_case "$name" /tmp/dylint-acceptance/a' in script
    assert (
        "The cold case intentionally owns first-time cargo-dylint and driver" in script
    )


def test_cross_worktree_registration_trace_is_collected() -> None:
    script = dylint_acceptance.BASH
    assert (
        "ZCCACHE_INNER_TRACE=/tmp/dylint-acceptance/diagnostics/"
        "context-registration-trace.jsonl"
    ) in script


def test_watchdog_symbol_smoke_uses_full_debug_info_and_real_gdb_attach() -> None:
    script = dylint_acceptance.BASH
    assert "run_symbolized_watchdog_smoke" in script
    assert 'dump_one_pid "$command_pid"' in script
    assert "watchdog-symbol-smoke-stacks.txt" in script
    assert "watchdog-symbol-smoke-passed" in script
    assert "grep -Fq 'exe=/target/debug/soldr'" in script
    assert "grep -Eq 'soldr_(cli|daemon|core)::'" in script
    assert (
        "grep -Eq 'crates/soldr-(cli|daemon|core)/src/" "[^ ]*\\.rs:[0-9]+'" in script
    )

    source = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    ).read_text(encoding="utf-8")
    assert '"profile.dev.debug=2"' in source


def test_acceptance_uses_the_per_checkout_perf_runner_identity() -> None:
    source = (
        Path(__file__).parents[1] / ".github" / "scripts" / "dylint_cache_acceptance.py"
    ).read_text(encoding="utf-8")
    assert 'runpy.run_path(str(ROOT / "ci" / "perf_local.py"))' in source
    assert "runner_container = runner.container" in source
    assert '"soldr-perf-local"' not in source
