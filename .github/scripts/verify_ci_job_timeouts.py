"""Verify that every job which allocates a runner has a bounded runtime.

soldr#1978 item 8. This used to check `ci.yml` alone, which left the two jobs
that run on *every* PR -- `_build-and-test.yml` and `_bootstrap-e2e.yml` --
sitting on GitHub's 360-minute default until soldr#1977 added timeouts by hand.

The per-job rule was already right: a job with `runs-on` needs a timeout, and a
reusable-workflow *caller* (`uses:` with no `runs-on`) cannot have one. What was
missing is that the reusable workflow *files themselves* contain jobs with
`runs-on`, and nothing ever opened them. Walking the whole directory makes the
regression class impossible rather than fixed-once.
"""

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


# Workflows that predate this check and still have unbounded jobs.
#
# soldr#1978 item 8: extending the walk to every workflow surfaced 26 jobs
# across these 7 files sitting on GitHub's 360-minute default. They are
# recorded rather than silently skipped, and rather than blocked -- the same
# trade `loc_ratchet` makes for the 13 files already over the line ceiling.
#
# The point of the extension is that a *new* workflow cannot join this list.
# Removing an entry is a burn-down PR; picking the right bound for a perf or
# release job needs someone who knows how long it legitimately runs, and a
# too-low timeout that kills a healthy long build is worse than the default.
GRANDFATHERED = frozenset(
    {
        "perf-cold-warm.yml",
        "perf-matrix.yml",
        "release-auto.yml",
        "vcpkg-windows-refresh.yml",
    }
)


def workflow_paths(root: Path) -> list[Path]:
    """Every workflow file, including reusable ones.

    Sorted so failure output is stable across runs and platforms.
    """

    return sorted(
        path
        for pattern in ("*.yml", "*.yaml")
        for path in root.glob(pattern)
    )


def main(workflow_path: Path | None = None) -> int:
    if workflow_path is not None:
        paths = [workflow_path]
    else:
        workflows = Path(__file__).resolve().parents[2] / ".github" / "workflows"
        paths = workflow_paths(workflows)
        if not paths:
            print(f"no workflows found under {workflows}", file=sys.stderr)
            return 1

    failed = False
    skipped = 0
    for path in paths:
        violations = find_timeout_violations(path.read_text(encoding="utf-8"))
        if violations and path.name in GRANDFATHERED:
            skipped += 1
            print(
                f"note: {path.name} has {len(violations)} unbounded job(s), "
                "grandfathered by soldr#1978 item 8"
            )
            continue
        if violations:
            failed = True
            print(f"workflow timeout policy failed: {path}", file=sys.stderr)
            for violation in violations:
                print(f"- {violation}", file=sys.stderr)
    if failed:
        return 1

    print(
        f"workflow timeout policy passed: {len(paths)} workflow(s)"
        + (f", {skipped} grandfathered" if skipped else "")
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
