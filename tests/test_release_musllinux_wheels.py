from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
RELEASE_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"


def test_release_workflow_builds_and_publishes_musllinux_wheels() -> None:
    workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

    assert "py3-none-musllinux_1_2_x86_64.whl" in workflow
    assert "py3-none-musllinux_1_2_aarch64.whl" in workflow
    # soldr#2294: the x64 musl wheel is built by `soldr wheel`, whose
    # `compatibility_for_target` policy (wheel_cmd.rs) tags release cross
    # builds musllinux_1_2; the native ARM musl lane still drives maturin
    # directly and pins the tag by hand.
    assert '"$driver" wheel --release --target "${{ matrix.target }}"' in workflow
    assert "--compatibility musllinux_1_2" in workflow
    assert "Build ARM musllinux wheel natively" in workflow
    assert "pypi-soldr-aarch64-unknown-linux-musl" in workflow
    assert "Assert linux-musl wheels are tagged musllinux_1_2" in workflow
    assert "Smoke test musllinux wheel on Alpine" in workflow
    assert "Smoke test native ARM musllinux wheel on Alpine" in workflow
    assert "alpine:3.20" in workflow
    assert "--only-binary=:all:" in workflow
    native_arm_wheel = workflow.split(
        "Smoke test native ARM musllinux wheel on Alpine", 1
    )[1]
    assert (
        'pip install --no-index --only-binary=:all: --find-links /dist "soldr==${EXPECTED_VERSION}"'
        in native_arm_wheel
    )
    assert "uv pip install --python .venv dist/*.whl" not in native_arm_wheel
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
