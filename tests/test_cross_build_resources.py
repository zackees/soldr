from __future__ import annotations

import subprocess
import sys
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "cross_build_resources.py"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"

_resources = load_script_module(SCRIPT, "cross_build_resources")


def test_every_archive_is_serialized() -> None:
    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-unknown-linux-gnu",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ]:
        assert _resources.archive_jobs(target) == 1


def test_cli_writes_both_cargo_and_soldr_limits(tmp_path: Path) -> None:
    github_env = tmp_path / "github-env"
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--target",
            "x86_64-unknown-linux-musl",
            "--github-env",
            str(github_env),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr
    assert github_env.read_text(encoding="utf-8").splitlines() == [
        "CARGO_BUILD_JOBS=1",
        "SOLDR_JOBS=1",
    ]


def test_workflow_applies_resource_policy_before_archive_build() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    policy = "      - name: Select nextest archive resources\n"
    archive = "      - name: Build nextest archive\n"
    assert policy in workflow
    assert workflow.index(policy) < workflow.index(archive)
    policy_block = workflow[workflow.index(policy) : workflow.index(archive)]
    assert "python3 .github/scripts/cross_build_resources.py" in policy_block
    assert "--target '${{ inputs.target }}'" in policy_block
    assert '--github-env "$GITHUB_ENV"' in policy_block
