from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    assert "py3-none-musllinux_1_2_x86_64.whl" in workflow
    assert "py3-none-musllinux_1_2_aarch64.whl" in workflow
    assert "compat_args=(--compatibility musllinux_1_2)" in workflow
    assert "Assert linux-musl wheels are tagged musllinux_1_2" in workflow
    assert "Smoke test musllinux wheel on Alpine" in workflow
    assert "alpine:3.20" in workflow
    assert "--only-binary=:all:" in workflow
    assert "expected=7" in workflow

    stale_markers = [
        "PyPI wheel build is gnu-only for now",
        "musl rows skip wheel build by design",
        "expected=5",
        "6 platform wheels",
    ]
    for marker in stale_markers:
        assert marker not in workflow

