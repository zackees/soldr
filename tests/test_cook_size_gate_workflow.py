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
    """The cook step keeps jobs=1 for its rung-4 reason; the build is uncapped.

    The ci-release build ran capped only while the pinned soldr predated
    soldr#3211 (soldr#3210: PDEATHSIG killed long compiler children when their
    spawning thread retired). The 0.9.16 pin carries the fix (soldr#3148). The
    cook step: ZCCACHE_DISABLE=1 leaves no admission gate (CLAUDE.md, rung 4).
    """

    import yaml

    steps = {
        step.get("name"): step
        for step in yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "cook-size-gate"
        ]["steps"]
    }
    build_env = steps["Build soldr CLI (ci-release)"].get("env") or {}
    assert "CARGO_BUILD_JOBS" not in build_env
    assert "SOLDR_JOBS" not in build_env
    assert "soldr#3211" in WORKFLOW.read_text(encoding="utf-8")
    cook_env = steps["Run soldr cook against zccache (release profile)"]["env"]
    assert cook_env["ZCCACHE_DISABLE"] == "1"
    assert cook_env["CARGO_BUILD_JOBS"] == "1"
    assert cook_env["SOLDR_JOBS"] == "1"


def test_fixture_is_pinned_to_the_embedded_zccache_version() -> None:
    """The cook fixture must be an immutable ref, and the one soldr embeds.

    soldr#3301: the fixture was checked out at `ref: main`, so an unrelated
    zccache merge (03d6ebb, which deleted `vendor/`) turned the gate red on
    every open PR, and the size thresholds were being asserted against a tree
    that changed under them. Tying the ref to `Cargo.lock` means a zccache
    dependency bump is what moves the fixture, deliberately and reviewably.
    """
    import re
    import tomllib

    workflow = WORKFLOW.read_text(encoding="utf-8")
    fixture_step = workflow[workflow.index("Checkout zccache fixture") :]
    ref = re.search(r'^\s+ref: "?([^"\n]+)"?', fixture_step, re.MULTILINE)
    assert ref, "cook fixture checkout declares no ref"
    pinned = ref.group(1).strip()
    assert pinned not in {
        "main",
        "master",
        "HEAD",
    }, f"cook fixture must not track a moving branch, got {pinned!r}"

    lock = tomllib.loads((REPO_ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    embedded = [p["version"] for p in lock["package"] if p["name"] == "zccache"]
    assert embedded, "Cargo.lock has no zccache package"
    assert pinned == embedded[0], (
        f"cook fixture is pinned to zccache {pinned}, but Cargo.lock embeds "
        f"{embedded[0]}; bump the fixture ref with the dependency"
    )
