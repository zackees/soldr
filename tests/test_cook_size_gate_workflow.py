"""Regression guards for the cook-size workflow runtime boundary."""

from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github/workflows/cook-size-gate.yml"


def test_cook_size_gate_bounds_build_and_isolates_the_measured_runtime() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert 'CARGO_BUILD_JOBS: "1"' in workflow
    assert 'SOLDR_JOBS: "1"' in workflow
    cook_step = workflow[workflow.index("Run soldr cook against zccache"):]
    assert 'SOLDR_BINARY: ${{ github.workspace }}/soldr/target/ci-release/soldr' in cook_step
    assert 'SOLDR_CACHE_DIR: ${{ runner.temp }}/cook-size-soldr' in cook_step
    assert 'ZCCACHE_CACHE_DIR: ${{ runner.temp }}/cook-size-soldr/cache/zccache' in cook_step
    assert 'ZCCACHE_DISABLE: "1"' in cook_step
