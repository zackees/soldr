"""Regression tests for the direct CI job timeout policy."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_ci_job_timeouts.py"
VERIFY = load_script_module(SCRIPT_PATH, "verify_ci_job_timeouts")


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


def test_inputs_gated_conditional_between_two_integers_is_accepted() -> None:
    # soldr#2615: _ci-target-run.yml grows its budget only when the PEP 517
    # smoke rides along. Both branches are static and range-checked.
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    timeout-minutes: ${{ inputs.run_pep517_smoke && 65 || 30 }}
    steps:
      - run: echo ok
"""
    assert VERIFY.find_timeout_violations(workflow) == []


def test_multiple_input_gates_with_static_integer_outcomes_are_accepted() -> None:
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    timeout-minutes: ${{ inputs.run_smoke && 65 || inputs.extended && 55 || 30 }}
    steps:
      - run: echo ok
"""
    assert VERIFY.find_timeout_violations(workflow) == []


def test_inputs_string_equality_gate_is_accepted() -> None:
    # soldr#3076: _ci-target-run.yml selects a budget for the macOS Recovery
    # guest mode by a string input rather than a boolean one. The comparison
    # is against a quoted literal, so every branch is still static.
    workflow = """\
jobs:
  direct:
    runs-on: ubuntu-latest
    timeout-minutes: ${{ inputs.target_execution == 'x86_64-recovery' && 30 || inputs.run_pep517_smoke && 65 || 30 }}
    steps:
      - run: echo ok
"""
    assert VERIFY.find_timeout_violations(workflow) == []


def test_conditional_with_out_of_range_branch_is_rejected() -> None:
    for value in (
        "${{ inputs.flag && 361 || 30 }}",
        "${{ inputs.flag && 65 || 0 }}",
        "${{ inputs.flag && 65 || matrix.slow }}",
        "${{ github.ref_name == 'main' && 65 || 30 }}",
    ):
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
    reusable = (
        "jobs:\n  inner:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: true\n"
    )
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


def test_grandfathered_entries_still_need_the_exemption():
    """An entry whose jobs are all bounded is an exemption nobody is using.

    `test_grandfathered_entries_still_exist` only catches a *deleted* workflow.
    A file that is still present but has since had every job bounded would keep
    its exemption forever, silently granting future unbounded jobs in that file
    a pass -- the opposite of a ratchet. Whoever bounds the last job should also
    delete the line, and this is what tells them to.
    """
    workflows = REPO_ROOT / ".github" / "workflows"
    fully_bounded = [
        name
        for name in sorted(VERIFY.GRANDFATHERED)
        if not VERIFY.find_timeout_violations(
            (workflows / name).read_text(encoding="utf-8")
        )
    ]
    assert not fully_bounded, (
        "these workflows no longer have unbounded jobs, so their GRANDFATHERED "
        f"entries should be removed (that is the burn-down): {fully_bounded}"
    )
