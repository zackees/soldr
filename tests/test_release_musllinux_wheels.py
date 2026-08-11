from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    # soldr#2453: 3-target native release ships only the x86_64 musl
    # wheel (static binary covers all Linux). The aarch64 musl wheel
    # is a documented exclusion.
    assert "py3-none-musllinux_1_2_x86_64.whl" in workflow
    assert "Smoke test musllinux wheel on Alpine" in workflow
    assert "alpine:3.20" in workflow
    assert "--only-binary=:all:" in workflow
    # 3 wheels: musl-x64, mac-arm64, win-x64
    assert "expected=3" in workflow

    stale_markers = [
        "PyPI wheel build is gnu-only for now",
        "musl rows skip wheel build by design",
        "expected=5",
        "6 platform wheels",
        "7 platform wheels",
        "expected=8",
    ]
    for marker in stale_markers:
        assert marker not in workflow
