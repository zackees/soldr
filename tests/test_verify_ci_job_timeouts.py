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
