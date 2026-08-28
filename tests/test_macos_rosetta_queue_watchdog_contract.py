"""Static workflow contracts for the #2968 Rosetta replay and queue observer."""

from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).parents[1]
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
TARGET_RUN = (ROOT / ".github" / "workflows" / "_ci-target-run.yml").read_text(encoding="utf-8")


def job_block(name: str, next_name: str) -> str:
    return CI.split(f"  {name}:\n", 1)[1].split(f"  {next_name}:\n", 1)[0]


def test_x86_macos_replay_requires_rosetta_on_arm_runner() -> None:
    replay = job_block("e2e-macos-x64", "e2e-macos-x64-queue-watchdog")
    assert "runs_on: macos-15" in replay
    assert "target_execution: x86_64-rosetta" in replay
    assert "macos-15-intel" not in replay
    assert "run_target_command.py --execution x86_64-rosetta --preflight" in TARGET_RUN
    assert "run_target_command.py --execution '${{ inputs.target_execution }}'" in TARGET_RUN


def test_queue_watchdog_observes_without_needing_target_run() -> None:
    observer = job_block("e2e-macos-x64-queue-watchdog", "e2e-macos-arm64-build")
    assert "needs: e2e-macos-x64-build" in observer
    assert "e2e-macos-x64" not in observer.replace("e2e-macos-x64-build", "")
    assert "runs-on: ubuntu-24.04" in observer
    assert "target_run_queue_watchdog.py" in observer
    assert "--grace-seconds 900" in observer
