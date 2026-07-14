"""Verify that direct jobs in the main CI workflow have bounded runtimes."""

from __future__ import annotations

import re
import sys
from pathlib import Path


JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):\s*(?:#.*)?$")
TOP_LEVEL_KEY = re.compile(r"^    ([A-Za-z0-9_-]+):(?:\s*(.*))?$")
INTEGER = re.compile(r"^[0-9]+$")


def _job_blocks(workflow: str) -> list[tuple[str, list[str]]]:
    """Return top-level job blocks from the workflow's ``jobs`` section."""

    in_jobs = False
    current_id: str | None = None
    current_lines: list[str] = []
    blocks: list[tuple[str, list[str]]] = []

    for line in workflow.splitlines():
        if not in_jobs:
            if line == "jobs:":
                in_jobs = True
            continue

        # A non-indented line starts a new YAML top-level section.
        if line and not line.startswith(" "):
            break

        match = JOB_HEADER.match(line)
        if match:
            if current_id is not None:
                blocks.append((current_id, current_lines))
            current_id = match.group(1)
            current_lines = []
        elif current_id is not None:
            current_lines.append(line)

    if current_id is not None:
        blocks.append((current_id, current_lines))
    return blocks


def _timeout_values(lines: list[str]) -> list[str]:
    """Read only four-space job keys, excluding nested step keys."""

    values: list[str] = []
    for line in lines:
        match = TOP_LEVEL_KEY.match(line)
        if match and match.group(1) == "timeout-minutes":
            values.append((match.group(2) or "").split("#", 1)[0].strip())
    return values


def find_timeout_violations(workflow: str) -> list[str]:
    """Return actionable errors for direct jobs without valid job timeouts."""

    violations: list[str] = []
    for job_id, lines in _job_blocks(workflow):
        keys = {
            match.group(1) for line in lines if (match := TOP_LEVEL_KEY.match(line))
        }
        if "uses" in keys and "runs-on" not in keys:
            # Reusable-workflow callers cannot use jobs.<id>.timeout-minutes.
            continue
        if "runs-on" not in keys:
            continue

        values = _timeout_values(lines)
        if not values:
            violations.append(f"{job_id}: missing job-level timeout-minutes")
            continue
        if len(values) != 1:
            violations.append(
                f"{job_id}: expected exactly one job-level timeout-minutes, found {len(values)}"
            )
            continue
        value = values[0]
        if not INTEGER.fullmatch(value) or not 1 <= int(value) <= 360:
            violations.append(
                f"{job_id}: timeout-minutes must be an integer from 1 to 360 (got {value!r})"
            )
    return violations


def main(workflow_path: Path | None = None) -> int:
    path = (
        workflow_path
        or Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"
    )
    violations = find_timeout_violations(path.read_text(encoding="utf-8"))
    if violations:
        print(f"workflow timeout policy failed: {path}", file=sys.stderr)
        for violation in violations:
            print(f"- {violation}", file=sys.stderr)
        return 1

    print(f"workflow timeout policy passed: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
