"""Regression tests for the direct CI job timeout policy."""

from __future__ import annotations

import importlib.util
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_ci_job_timeouts.py"
SPEC = importlib.util.spec_from_file_location("verify_ci_job_timeouts", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
VERIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFY)


def test_current_ci_direct_jobs_have_valid_timeouts() -> None:
    workflow = (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text(
        encoding="utf-8"
    )

    assert VERIFY.find_timeout_violations(workflow) == []


def test_missing_timeout_names_the_direct_job() -> None:
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    steps:
      - run: echo ok
  reusable:
    uses: ./.github/workflows/reusable.yml
"""

    assert VERIFY.find_timeout_violations(workflow) == [
        "direct: missing job-level timeout-minutes"
    ]


def test_invalid_timeout_values_are_rejected() -> None:
    for value in ("0", "-1", "not-a-number", "361"):
        workflow = f"""\
jobs:
  direct:
    runs-on: ubuntu-latest
    timeout-minutes: {value}
    steps:
      - run: echo ok
"""
        assert VERIFY.find_timeout_violations(workflow) == [
            f"direct: timeout-minutes must be an integer from 1 to 360 (got {value!r})"
        ]


def test_reusable_workflow_callers_are_not_required_to_have_timeout() -> None:
    workflow = """\
jobs:
  reusable:
    uses: ./.github/workflows/reusable.yml
"""

    assert VERIFY.find_timeout_violations(workflow) == []


def test_step_timeout_does_not_satisfy_job_timeout() -> None:
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    steps:
      - timeout-minutes: 5
        run: echo ok
"""

    assert VERIFY.find_timeout_violations(workflow) == [
        "direct: missing job-level timeout-minutes"
    ]


def test_duplicate_job_timeouts_are_rejected() -> None:
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    timeout-minutes: 5
    timeout-minutes: 10
    steps:
      - run: echo ok
"""

    assert VERIFY.find_timeout_violations(workflow) == [
        "direct: expected exactly one job-level timeout-minutes, found 2"
    ]


def test_reusable_workflow_own_job_is_checked():
    """soldr#1978 item 8: the regression that motivated widening the walk.

    A reusable-workflow *caller* is exempt (``timeout-minutes`` is invalid on a
    ``uses:`` job), but the reusable workflow's own job allocates a runner and
    must be bounded. Checking only ``ci.yml`` never opened those files, which is
    how ``_build-and-test.yml`` and ``_bootstrap-e2e.yml`` -- on every PR -- sat
    on GitHub's 360-minute default.
    """
    reusable = "jobs:\n  inner:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n"
    assert VERIFY.find_timeout_violations(reusable) == [
        "inner: missing job-level timeout-minutes"
    ]

    caller = "jobs:\n  call:\n    uses: ./.github/workflows/_x.yml\n"
    assert VERIFY.find_timeout_violations(caller) == []


def test_walk_covers_reusable_workflows():
    """The walk must reach the files the old default never opened."""
    paths = VERIFY.workflow_paths(REPO_ROOT / ".github" / "workflows")
    names = {path.name for path in paths}
    assert "_build-and-test.yml" in names
    assert "_bootstrap-e2e.yml" in names
    assert "ci.yml" in names


def test_grandfathered_entries_still_exist():
    """A grandfathered name that no longer exists is stale and hides nothing.

    Deleting a workflow should drop its entry, not leave a line implying a debt
    that is already gone.
    """
    workflows = REPO_ROOT / ".github" / "workflows"
    missing = [name for name in VERIFY.GRANDFATHERED if not (workflows / name).exists()]
    assert not missing, f"grandfathered workflows no longer present: {missing}"
