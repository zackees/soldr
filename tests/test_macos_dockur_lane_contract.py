"""No macos-* GitHub Actions runner exists anywhere (soldr#3071).

Owner mandate (2026-09-02): no job may run on a macos-* runner for building
or testing. macOS execution happens only inside a dockur/macos x86_64 guest
(ci/macos_x64_guest.py) hosted on an ordinary ubuntu-24.04 runner.

This replaces tests/test_macos_rosetta_queue_watchdog_contract.py, whose
contract (a Rosetta replay on a macos-15 runner, watched by a queue
watchdog) is gone: e2e-macos-x64 now runs on ubuntu-24.04 under
target_execution: x86_64-dockur, and e2e-macos-arm64 has no run job at all.
"""

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"

# A `runs-on:`/`runs_on:`/`runner:` YAML value (bare or quoted, matrix-row
# style or JSON-style `"runner": "..."`) naming a macos-* runner label.
# Comment-only lines are excluded below.
RUNNER_LABEL_PATTERN = re.compile(
    r'(?:runs[-_]on|"?runner"?)\s*:\s*"?macos-[a-z0-9.]+', re.IGNORECASE
)


def _non_comment_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if not line.strip().startswith("#")]


def test_no_workflow_names_a_macos_runner_label() -> None:
    offenders = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for line in _non_comment_lines(workflow.read_text(encoding="utf-8")):
            if RUNNER_LABEL_PATTERN.search(line):
                offenders.append(f"{workflow.name}: {line.strip()}")
    assert not offenders, (
        "no GitHub Actions job may run on a macos-* runner (owner mandate "
        f"2026-09-02, soldr#3071): {offenders}"
    )


def test_x64_lane_uses_the_dockur_guest_on_an_ubuntu_runner() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    start = ci.index("  e2e-macos-x64:\n")
    end = ci.index("\n  # ---------- macOS ARM64", start)
    run_job = ci[start:end]
    assert "runs_on: ubuntu-24.04" in run_job
    assert "target_execution: x86_64-dockur" in run_job
    assert "uses: ./.github/workflows/_ci-target-run.yml" in run_job


def test_no_macos_arm64_run_job_exists() -> None:
    ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    assert re.search(r"(?m)^  e2e-macos-arm64:\s*$", ci) is None
    # The build-only lane must still exist -- only its paired run job is gone.
    assert re.search(r"(?m)^  e2e-macos-arm64-build:\s*$", ci) is not None
