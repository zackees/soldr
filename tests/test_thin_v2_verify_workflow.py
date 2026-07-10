"""Regression tests for the thin-v2 fresh-target verifier workflow."""

from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "thin-v2-verify.yml"


def test_verifier_restores_into_an_empty_target_without_sentinel_bypass() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    first_build = workflow.index("First (cold) build")
    delete_target = workflow.index('rm -rf "${{ runner.temp }}/verify-noop/target"')
    second_build = workflow.index("Second (fresh-target restore)")

    assert first_build < delete_target < second_build
    assert 'SOLDR_RUST_PLAN_SKIP_WARM_RESTORE: "0"' in workflow[second_build:]
    assert 'cargo:rerun-if-changed=build-input.txt' in workflow


def test_verifier_runs_when_embedded_zccache_or_its_contract_tests_change() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert workflow.count('- "_vender/zccache"') == 2
    assert workflow.count('- "tests/test_thin_v2_verify_workflow.py"') == 2
