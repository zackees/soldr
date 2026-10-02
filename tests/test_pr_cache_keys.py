"""PR-context cache saves carry `pr-<N>` in their key (zackees/ci.yml#6).

The janitor (`check_cache_budget.py`) deletes a PR's caches once the PR is
closed, matching on the `refs/pull/<N>/*` ref or on the `pr-<N>` key tag. The
guard (`check_pr_cache_keys.py`) makes sure no PR-reachable save escapes
without the tag, and that the one expression producing it is defined once per
workflow, identically.
"""

from __future__ import annotations

import re
from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
guard = load_script_module(SCRIPTS / "check_pr_cache_keys.py", "check_pr_cache_keys")
budget = load_script_module(
    SCRIPTS / "check_cache_budget.py", "check_cache_budget_keys"
)

CACHE = "actions/cache@0400d5f644dc74513175e3cd8d07132dd4860809"


def evaluate_tag(event_name: str, pr_number: int | None) -> str:
    """Evaluate `PR_CACHE_TAG` the way GitHub's `a && b || c` does."""
    match = re.fullmatch(
        r"\$\{\{ github\.event_name == 'pull_request' && "
        r"format\('(?P<fmt>[^']*)', github\.event\.pull_request\.number\) "
        r"\|\| '' \}\}",
        guard.TAG_EXPRESSION,
    )
    assert match, guard.TAG_EXPRESSION
    if event_name == "pull_request":
        return match["fmt"].replace("{0}", str(pr_number))
    return ""


def test_the_tag_is_pr_n_in_pr_context_and_empty_on_main() -> None:
    assert evaluate_tag("pull_request", 3438) == "-pr-3438"
    for event in ("push", "schedule", "workflow_dispatch"):
        assert evaluate_tag(event, None) == ""


def test_the_janitor_attributes_a_tagged_key_to_its_pr_only() -> None:
    key = f"bootstrap-soldr-blessed-linux-gnu-dev-v1{evaluate_tag('pull_request', 12)}-abc"
    entry = budget.CacheEntry(key, "refs/heads/main", 1)
    assert budget.pr_number_of(entry) == 12
    main_key = (
        f"bootstrap-soldr-blessed-linux-gnu-dev-v1{evaluate_tag('push', None)}-abc"
    )
    assert (
        budget.pr_number_of(budget.CacheEntry(main_key, "refs/heads/main", 1)) is None
    )
    # setup-uv appends its own `-` before a cache-suffix.
    uv_key = (
        f"setup-uv-1-x86_64-3.13-no-dependency-glob-{evaluate_tag('pull_request', 123)}"
    )
    assert budget.pr_number_of(budget.CacheEntry(uv_key, "refs/heads/main", 1)) == 123


def write(directory: Path, name: str, body: str) -> None:
    (directory / name).write_text(body, encoding="utf-8")


def test_an_untagged_pr_context_save_fails(tmp_path: Path) -> None:
    write(
        tmp_path,
        "ci.yml",
        f"""
on:
  pull_request:
jobs:
  build:
    steps:
      - uses: {CACHE}
        with:
          key: rustup-1.98.1
""",
    )
    errors = guard.check(tmp_path)
    assert any("build/0" in error for error in errors), errors
    assert any("define workflow-level env PR_CACHE_TAG" in error for error in errors)


def test_a_tagged_save_in_a_called_workflow_passes_and_untagged_fails(
    tmp_path: Path,
) -> None:
    write(
        tmp_path,
        "ci.yml",
        "on:\n  pull_request:\njobs:\n  call:\n    uses: ./.github/workflows/_reuse.yml\n",
    )
    body = f"""
on:
  workflow_call:
env:
  PR_CACHE_TAG: {guard.TAG_EXPRESSION}
jobs:
  build:
    steps:
      - uses: {CACHE}
        with:
          key: bin${{{{ env.PR_CACHE_TAG }}}}-sha
      - uses: astral-sh/setup-uv@abc
"""
    write(tmp_path, "_reuse.yml", body)
    errors = guard.check(tmp_path)
    assert errors == [
        f"{tmp_path / '_reuse.yml'}: build/1 can save a cache in PR context "
        "without ${{ env.PR_CACHE_TAG }} in its key"
    ]


def test_saves_that_skip_prs_need_no_tag(tmp_path: Path) -> None:
    write(
        tmp_path,
        "ci.yml",
        """
on:
  pull_request:
jobs:
  build:
    steps:
      - uses: actions/cache/restore@abc
        with:
          key: rustup
      - uses: actions/cache/save@abc
        if: github.event_name != 'pull_request'
        with:
          key: rustup
      - uses: astral-sh/setup-uv@abc
        with:
          enable-cache: false
""",
    )
    assert guard.check(tmp_path) == []


def test_workflows_not_reachable_from_a_pr_are_ignored(tmp_path: Path) -> None:
    write(
        tmp_path,
        "nightly.yml",
        f"on:\n  schedule:\n    - cron: '0 0 * * *'\njobs:\n  a:\n    steps:\n      - uses: {CACHE}\n",
    )
    write(
        tmp_path,
        "closed.yml",
        f"on:\n  pull_request:\n    types: [closed]\njobs:\n  a:\n    steps:\n      - uses: {CACHE}\n",
    )
    assert guard.check(tmp_path) == []


def test_the_real_workflow_tree_tags_every_pr_context_save() -> None:
    assert guard.check(REPO_ROOT / ".github" / "workflows") == []
