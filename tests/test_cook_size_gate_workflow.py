"""Regression guards for the cook-size workflow runtime boundary."""

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github/workflows/cook-size-gate.yml"


def test_cook_size_gate_bounds_build_and_isolates_the_measured_runtime() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert 'CARGO_BUILD_JOBS: "1"' in workflow
    assert 'SOLDR_JOBS: "1"' in workflow
    cook_step = workflow[workflow.index("Run soldr cook against zccache") :]
    assert (
        "SOLDR_BINARY: ${{ github.workspace }}/soldr/target/ci-release/soldr"
        in cook_step
    )
    assert "SOLDR_CACHE_DIR: ${{ runner.temp }}/cook-size-soldr" in cook_step
    assert (
        "ZCCACHE_CACHE_DIR: ${{ runner.temp }}/cook-size-soldr/cache/zccache"
        in cook_step
    )
    assert 'ZCCACHE_DISABLE: "1"' in cook_step


def test_cook_fixture_preparation_pins_uv_python_313() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    fixture_step = workflow[
        workflow.index("Prepare zccache fixture path dependencies") :
    ]
    assert "uv run --no-project --python 3.13 python" in fixture_step


def test_only_the_cache_disabled_cook_keeps_a_compile_cap() -> None:
    """soldr#3148: the wrapped build relies on swap + exclusive admission.

    The cook step runs with ZCCACHE_DISABLE=1, so no admission gate exists
    there and its cap stays (CLAUDE.md, concurrency ladder rung 4).
    """

    import yaml

    workflow = (
        Path(__file__).resolve().parents[1]
        / ".github"
        / "workflows"
        / "cook-size-gate.yml"
    )
    steps = {
        step.get("name"): step
        for step in yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"][
            "cook-size-gate"
        ]["steps"]
    }
    build_env = steps["Build soldr CLI (ci-release)"].get("env") or {}
    assert "CARGO_BUILD_JOBS" not in build_env
    assert "SOLDR_JOBS" not in build_env
    cook_env = steps["Run soldr cook against zccache (release profile)"]["env"]
    assert cook_env["ZCCACHE_DISABLE"] == "1"
    assert cook_env["CARGO_BUILD_JOBS"] == "1"
    assert cook_env["SOLDR_JOBS"] == "1"
