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
    ):
        assert scenario in source
    assert "test ! -e target/debug" in source
    assert "test ! -e target/release" in source


def test_watchdog_collects_native_and_async_diagnostics() -> None:
    source = SCRIPT.read_text(encoding="utf-8")
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "thread apply all bt full 64" in source
    assert "CARGO_PROFILE_DEV_DEBUG=2" in source
    assert r"\.(debug_info|debug_line|symtab)" in source
    assert "SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH" in source
    assert "dylint-cook-diagnostics" in source
    assert "dylint-cook-diagnostics" in workflow
