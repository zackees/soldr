"""Contract for target-run's lane-local rustup-target provisioning."""

from __future__ import annotations

import subprocess
import re
from pathlib import Path
from typing import Any

import pytest
import yaml
from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / ".github" / "scripts" / "target_run_add_target.py"
TARGET_RUN = REPO_ROOT / ".github" / "workflows" / "_ci-target-run.yml"
CI = REPO_ROOT / ".github" / "workflows" / "ci.yml"
TARGET = "wasm32-wasip1-threads"
REQUIRED_ENV = "SOLDR_REQUIRE_WASM32_WASIP1_THREADS_MATERIALIZATION"

helper = load_script_module(SCRIPT, "target_run_add_target")


def test_provisioning_uses_the_packaged_soldr_rustup_front_door() -> None:
    calls: list[tuple[object, object]] = []

    def record(command: object, *, check: object) -> None:
        calls.append((command, check))

    helper.provision_target(
        Path("packaged-soldr"),
        channel="1.95.0",
        target=TARGET,
        run=record,
    )

    assert calls == [
        (
            [
                "packaged-soldr",
                "rustup",
                "target",
                "add",
                TARGET,
                "--toolchain",
                "1.95.0",
            ],
            True,
        )
    ]


@pytest.mark.parametrize("value", ["", "  ", "\n"])
def test_blank_workflow_values_are_rejected(value: str) -> None:
    with pytest.raises(helper.argparse.ArgumentTypeError, match="must not be empty"):
        helper.required_value(value)


def test_subprocess_failure_is_reported_as_a_failed_helper(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    def fail(*_args: Any, **_kwargs: Any) -> None:
        raise subprocess.CalledProcessError(9, ["soldr"])

    monkeypatch.setattr(helper, "provision_target", fail)
    assert (
        helper.main(["--soldr", "soldr", "--channel", "1.95.0", "--target", TARGET])
        == 1
    )
    assert f"failed to provision {TARGET} through Soldr" in capsys.readouterr().err


def workflow_steps() -> list[dict[str, object]]:
    document = yaml.safe_load(TARGET_RUN.read_text(encoding="utf-8"))
    return next(iter(document["jobs"].values()))["steps"]


def test_authoritative_target_run_provisions_and_enforces_the_wasm_target() -> None:
    workflow = TARGET_RUN.read_text(encoding="utf-8")
    assert "require_wasm32_wasip1_threads_materialization:" in workflow
    assert REQUIRED_ENV in workflow
    step = next(
        step
        for step in workflow_steps()
        if step.get("name")
        == "Provision Wasm target for materialization regression (soldr#2919)"
    )
    assert step["if"] == "${{ inputs.require_wasm32_wasip1_threads_materialization }}"
    assert "target_run_add_target.py" in step["run"]
    assert '"$SOLDR_BIN"' in step["run"]
    assert '"$RUSTUP_TOOLCHAIN"' in step["run"]
    assert TARGET in step["run"]


def test_linux_and_windows_replays_opt_in_but_macos_does_not() -> None:
    workflow = CI.read_text(encoding="utf-8")
    required_jobs = (
        "e2e-linux-arm64",
        "e2e-linux-arm64-musl",
        "e2e-windows-x64",
        "e2e-windows-x64-gnu",
        "e2e-windows-arm64",
    )
    for job in required_jobs:
        block = job_block(workflow, job)
        assert "require_wasm32_wasip1_threads_materialization: true" in block
    for job in ("e2e-macos-x64", "e2e-macos-arm64"):
        block = job_block(workflow, job)
        assert "require_wasm32_wasip1_threads_materialization" not in block


def job_block(workflow: str, job: str) -> str:
    start = workflow.index(f"  {job}:\n")
    next_job = re.search(r"(?m)^  [a-zA-Z0-9_-]+:\n", workflow[start + 1 :])
    end = start + 1 + next_job.start() if next_job else len(workflow)
    return workflow[start:end]
