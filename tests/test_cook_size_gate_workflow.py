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


def test_both_compile_caps_state_their_rung_4_reason() -> None:
    """Both capped steps keep jobs=1, and each for a stated reason.

    The ci-release build: soldr#3210 (PDEATHSIG kills long compiler children
    when their spawning thread retires under concurrency). The cook step:
    ZCCACHE_DISABLE=1 leaves no admission gate (CLAUDE.md, ladder rung 4).
    """

    import yaml

    steps = {
        step.get("name"): step
        for step in yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "cook-size-gate"
        ]["steps"]
    }
    build_env = steps["Build soldr CLI (ci-release)"]["env"]
    assert build_env["CARGO_BUILD_JOBS"] == "1"
    assert build_env["SOLDR_JOBS"] == "1"
    assert "soldr#3210" in WORKFLOW.read_text(encoding="utf-8")
    cook_env = steps["Run soldr cook against zccache (release profile)"]["env"]
    assert cook_env["ZCCACHE_DISABLE"] == "1"
    assert cook_env["CARGO_BUILD_JOBS"] == "1"
    assert cook_env["SOLDR_JOBS"] == "1"
