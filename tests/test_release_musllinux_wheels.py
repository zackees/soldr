from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    assert "py3-none-musllinux_1_2_x86_64.whl" in workflow
    assert "py3-none-musllinux_1_2_aarch64.whl" in workflow
    assert (
        "uses: zackees/setup-soldr@62d1596b70168e422156f12273a2ed476d3a16dc" in workflow
    )
    assert "version: 0.8.44" in workflow
    assert "cross-targets: ${{ matrix.target }}" in workflow
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
