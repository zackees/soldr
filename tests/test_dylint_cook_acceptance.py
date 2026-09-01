from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".github" / "scripts" / "dylint_cook_acceptance.py"
WORKFLOW = ROOT / ".github" / "workflows" / "dylint-cook-acceptance.yml"


def test_acceptance_covers_restore_and_object_cache_scenarios() -> None:
    source = SCRIPT.read_text(encoding="utf-8")
    for scenario in (
        "cold",
        "warm_same_target",
        "warm_restored_target",
        "object_cache_only",
        "tests_cold",
        "tests_warm_restored_target",
        "tests_object_cache_only",
    ):
        assert scenario in source
    assert "unset CARGO_TARGET_DIR" in source
    assert "test ! -e target/debug" in source
    assert "test ! -e target/release" in source


def test_the_tests_tree_matrix_asserts_a_cook_level_skip_not_an_object_hit() -> None:
    """A warm Tier-2 object store must never be mistaken for a working cook.

    If a future change let a warm object store turn `tests_object_cache_only`
    into a `skip`, the cook tier's dependency-layer restore would be
    silently untested: the run would still pass, but for the wrong reason
    (per-unit object hits papering over a cook miss) instead of the cook
    tier actually avoiding the work.
    """
    source = SCRIPT.read_text(encoding="utf-8")
    assert "--tree tests" in source
    assert "tests_object_cache_only" in source
    assert '"tests_object_cache_only": "miss",' in source
    assert '"tests_warm_restored_target": "skip",' in source


def test_watchdog_collects_native_and_async_diagnostics() -> None:
    source = SCRIPT.read_text(encoding="utf-8")
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "thread apply all bt full 64" in source
    assert "CARGO_PROFILE_DEV_DEBUG=2" in source
    assert r"\.(debug_info|debug_line|symtab)" in source
    assert "SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH" in source
    assert "SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS=20" in source
    assert "dylint-cook-diagnostics" in source
    assert "dylint-cook-diagnostics" in workflow
    assert source.index('"docker",\n            "cp"') < source.index("if returncode:")


def test_acceptance_uses_the_per_checkout_perf_runner_identity() -> None:
    source = SCRIPT.read_text(encoding="utf-8")
    assert 'runpy.run_path(str(ROOT / "ci" / "perf_local.py"))' in source
    assert "runner_container = runner.container" in source
    assert '"soldr-perf-local"' not in source
