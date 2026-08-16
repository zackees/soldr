from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    # soldr#2469 step 2.2: expected wheel names are generated from
    # ci/canonical-targets.json via release_completeness.py (whose parity
    # test pins the musllinux_1_2 tags); the workflow gates must invoke
    # the generator instead of carrying inline name lists.
    assert workflow.count("--list-expected-github-assets") == 2
    assert (
        "uses: zackees/setup-soldr@40320d277ba4946e38d4b3c02e6c7a15a29c3f3f" in workflow
    )
    assert "version: 0.8.44" in workflow
    assert "cross-targets: ${{ matrix.setup_target }}" in workflow
    assert "target-wheel-hook" in workflow
    assert ".github/scripts/build_release_wheel.py" in workflow
    assert "target: x86_64-unknown-linux-musl" in workflow
    assert "target: aarch64-unknown-linux-musl" in workflow
    assert "name: pypi-soldr-${{ matrix.target }}" in workflow
    assert "Assert linux-musl wheels are tagged musllinux_1_2" in workflow
    assert "Smoke test musllinux wheel on Alpine" in workflow
    assert "alpine:3.20" in workflow
    assert "--only-binary=:all:" in workflow
    alpine_smoke = workflow.split("Smoke test musllinux wheel on Alpine", 1)[1].split(
        "Smoke test standalone musl binary", 1
    )[0]
    assert (
        'pip install --no-index --only-binary=:all: --find-links /dist "soldr==${EXPECTED_VERSION}"'
        in alpine_smoke
    )
    assert "uv pip install --python .venv dist/*.whl" not in alpine_smoke
    assert "expected=8" in workflow

    stale_markers = [
        "PyPI wheel build is gnu-only for now",
        "musl rows skip wheel build by design",
        "expected=5",
        "6 platform wheels",
        "7 platform wheels",
    ]
    for marker in stale_markers:
        assert marker not in workflow
