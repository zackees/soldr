"""Regression locks for the CI step-duration wrapper.

soldr#2996 makes this wrapper load-bearing on `soldr ci-test`, which is
~41 minutes of a ~45 minute job -- the single largest item in CI. A
wrapper on that step must never be the reason the step fails, and must
never swallow a failure that did happen.
"""

import os
import sys
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"


def _module():
    return load_script_module(SCRIPTS / "step_duration.py", "step_duration")


def test_exit_code_of_a_successful_command_is_passed_through() -> None:
    module = _module()
    assert module.main(["--label", "ok", "--", sys.executable, "-c", "pass"]) == 0


def test_a_failing_command_still_fails_the_step() -> None:
    """The wrapper must not turn a red step green."""
    module = _module()
    code = module.main(
        ["--label", "boom", "--", sys.executable, "-c", "raise SystemExit(7)"]
    )
    assert code == 7


def test_duration_is_appended_to_the_job_step_summary(tmp_path) -> None:
    module = _module()
    summary = tmp_path / "summary.md"
    os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
    try:
        module.main(["--label", "target / soldr ci-test", "--", sys.executable, "-c", "pass"])
    finally:
        del os.environ["GITHUB_STEP_SUMMARY"]
    body = summary.read_text(encoding="utf-8")
    assert "**target / soldr ci-test**" in body
    assert body.rstrip().endswith("s")


def test_summary_is_written_even_when_the_command_failed(tmp_path) -> None:
    """A slow step that then failed is exactly when the timing is worth having."""
    module = _module()
    summary = tmp_path / "summary.md"
    os.environ["GITHUB_STEP_SUMMARY"] = str(summary)
    try:
        code = module.main(
            ["--label", "failed step", "--", sys.executable, "-c", "raise SystemExit(3)"]
        )
    finally:
        del os.environ["GITHUB_STEP_SUMMARY"]
    assert code == 3
    assert "**failed step**" in summary.read_text(encoding="utf-8")


def test_an_unwritable_summary_never_fails_the_lane(tmp_path) -> None:
    """The summary is a convenience; it must not gate the build."""
    module = _module()
    # A path whose parent does not exist -- open() raises, and the wrapper
    # is required to swallow it.
    os.environ["GITHUB_STEP_SUMMARY"] = str(tmp_path / "missing-dir" / "summary.md")
    try:
        assert module.main(["--label", "x", "--", sys.executable, "-c", "pass"]) == 0
    finally:
        del os.environ["GITHUB_STEP_SUMMARY"]


def test_absent_summary_env_is_not_an_error() -> None:
    """Runs outside Actions (a developer shell) must work unchanged."""
    module = _module()
    os.environ.pop("GITHUB_STEP_SUMMARY", None)
    assert module.main(["--label", "x", "--", sys.executable, "-c", "pass"]) == 0
