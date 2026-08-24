"""The interpreter-pin guard must catch an unpinned job, and only that (soldr#2763).

The bug it guards: a workflow job runs `.github/scripts/*.py` under whatever
`python3` the runner image happens to ship. That killed the v0.9.3 release on
macOS ARM64 with `'PosixPath' object has no attribute 'hardlink_to'` -- a 3.10+
API on an image whose python3 was older.

Two directions matter here and they fail differently. Missing a genuinely
unpinned job lets the release bug back in. Flagging a pinned one -- or a job
that runs no repo Python at all -- makes the guard noise, and a noisy guard gets
baselined into uselessness, which is the same outcome more slowly.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "check_workflow_python_pin.py"
)

SETUP_UV = "astral-sh/setup-uv@e58605a9b6da7c637471fab8847a5e5a6b8df081 # v5"
SETUP_PYTHON = "actions/setup-python@a309ff8b426b58ec0e2a45f0f869d46889d02405 # v6.2.0"


@pytest.fixture(scope="module")
def guard():
    return load_script_module(SCRIPT, "check_workflow_python_pin")


def write_workflow(directory: Path, name: str, body: str) -> Path:
    path = directory / name
    path.write_text(body, encoding="utf-8")
    return path


# --------------------------- catching the real thing ---------------------------


def test_unpinned_job_running_a_repo_script_is_flagged(guard, tmp_path):
    write_workflow(
        tmp_path,
        "release.yml",
        """
jobs:
  build:
    runs-on: macos-14
    steps:
      - run: python3 .github/scripts/stage_release_binaries.py --out dist
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {("release.yml", "build")}


def test_ci_directory_scripts_count_too(guard, tmp_path):
    """`ci/*.py` runs on the same runners and has the same floor."""
    write_workflow(
        tmp_path,
        "perf.yml",
        """
jobs:
  gate:
    runs-on: ubuntu-24.04
    steps:
      - run: python3 ci/perf_local.py cargo build
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {("perf.yml", "gate")}


def test_every_unpinned_job_in_a_file_is_reported(guard, tmp_path):
    write_workflow(
        tmp_path,
        "many.yml",
        """
jobs:
  first:
    steps:
      - run: python3 ci/one.py
  second:
    steps:
      - run: python3 .github/scripts/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {
        ("many.yml", "first"),
        ("many.yml", "second"),
    }


# ------------------------------ not over-firing --------------------------------


def test_setup_python_pins_a_bare_python3_call(guard, tmp_path):
    """setup-python prepends its interpreter to PATH, so `python3` resolves to it."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_PYTHON}
      - run: python3 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_setup_uv_pins_an_invocation_routed_through_uv_run(guard, tmp_path):
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_setup_uv_does_not_pin_a_bare_python3_call(guard, tmp_path):
    """The distinction the loose version of this guard got wrong.

    Installing uv does not change what `python3` means. A job that sets uv up
    and then calls `python3 script.py` is exactly as exposed as one that set up
    nothing -- but it *looks* pinned, which is worse, because a reader checking
    for a setup step finds one. Two jobs in this repo were in that state.
    """
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: python3 .github/scripts/stage_release_binaries.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {("release.yml", "build")}


def test_one_uv_run_does_not_cover_a_sibling_bare_call(guard, tmp_path):
    """Partial adoption is the state a half-finished migration leaves behind."""
    write_workflow(
        tmp_path,
        "release.yml",
        f"""
jobs:
  build:
    steps:
      - uses: {SETUP_UV}
      - run: uv run --python 3.13 .github/scripts/one.py
      - run: python3 .github/scripts/two.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {("release.yml", "build")}


def test_a_container_job_is_not_flagged(guard, tmp_path):
    """The image is pinned in the workflow, so the interpreter cannot drift."""
    write_workflow(
        tmp_path,
        "cross.yml",
        """
jobs:
  build:
    container:
      image: ghcr.io/zackees/soldr-cross:pinned
    steps:
      - run: python3 ci/build.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_a_job_running_no_repo_python_is_ignored(guard, tmp_path):
    """Including one that runs Python -- the floor only binds on repo scripts."""
    write_workflow(
        tmp_path,
        "build.yml",
        """
jobs:
  build:
    steps:
      - run: cargo build --workspace
      - run: python3 -c "print('inline is not a repo script')"
      - run: python3 /usr/share/tool/vendored.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == set()


def test_pinning_one_job_does_not_excuse_its_neighbour(guard, tmp_path):
    """A setup step is per job; a sibling job on another runner gets nothing."""
    write_workflow(
        tmp_path,
        "two.yml",
        f"""
jobs:
  pinned:
    steps:
      - uses: {SETUP_PYTHON}
      - run: python3 ci/a.py
  unpinned:
    steps:
      - run: python3 ci/b.py
""",
    )
    assert guard.unpinned_jobs(tmp_path) == {("two.yml", "unpinned")}


# ---------------------------- the baseline is honest ---------------------------


def test_baseline_matches_the_repository_exactly(guard):
    """Both directions, or the list rots.

    An unlisted unpinned job is the release bug walking back in. A listed job
    that has since been pinned makes the baseline a record of solved problems,
    and the next reader cannot tell which entries still mean anything.
    """
    found = guard.unpinned_jobs(guard.WORKFLOW_DIR)
    assert found - guard.BASELINE == set(), "unpinned job missing from BASELINE"
    assert guard.BASELINE - found == set(), "BASELINE entry is no longer unpinned"


def test_baseline_entries_name_workflows_that_exist(guard):
    """A renamed or deleted workflow must not leave a silent entry behind."""
    for workflow in {entry[0] for entry in guard.BASELINE}:
        assert (
            guard.WORKFLOW_DIR / workflow
        ).is_file(), f"BASELINE names {workflow}, which does not exist"


# --------------------------------- exit codes ----------------------------------


def test_main_passes_when_only_baselined_jobs_are_unpinned(guard, monkeypatch):
    # main() parses the real sys.argv, which under pytest is pytest's own.
    monkeypatch.setattr("sys.argv", ["check"])
    assert guard.main() == 0


def test_main_fails_on_a_new_unpinned_job(guard, tmp_path, monkeypatch, capsys):
    write_workflow(
        tmp_path,
        "brand-new.yml",
        """
jobs:
  fresh:
    steps:
      - run: python3 .github/scripts/whatever.py
""",
    )
    monkeypatch.setattr("sys.argv", ["check", "--workflow-dir", str(tmp_path)])
    assert guard.main() == 1
    out = capsys.readouterr().out
    assert "brand-new.yml" in out and "fresh" in out
    # The message has to say what to do; a guard that only says "no" gets
    # worked around rather than satisfied.
    assert "setup-uv" in out
