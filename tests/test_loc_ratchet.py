"""Tests for the CI line-ceiling ratchet (soldr#1966).

These drive real git repositories rather than mocking `git`, because every
interesting case is about what the *merge base* held — which is precisely the
part a mock would assume rather than verify.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = Path(__file__).resolve().parents[1] / ".github" / "scripts" / "loc_ratchet.py"


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "loc_ratchet")


def _git(repo: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=repo, check=True, capture_output=True, text=True
    ).stdout


def _write(repo: Path, rel: str, lines: int) -> None:
    path = repo / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(f"// {i}" for i in range(lines)) + "\n", encoding="utf-8")


def _commit(repo: Path, message: str) -> None:
    _git(repo, "add", "-A")
    _git(repo, "commit", "-q", "-m", message)


def _evaluate(mod, repo: Path, ceiling: int = 100):
    cwd = Path.cwd()
    import os

    os.chdir(repo)
    try:
        return mod.evaluate("main", ("crates",), ceiling)
    finally:
        os.chdir(cwd)


def test_a_file_under_the_ceiling_passes(mod, repo):
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/a.rs", 20)
    _commit(repo, "grow a little")

    violations, checked = _evaluate(mod, repo)
    assert violations == []
    assert checked == 1


def test_crossing_the_ceiling_fails(mod, repo):
    _write(repo, "crates/a.rs", 50)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/a.rs", 150)
    _commit(repo, "cross the ceiling")

    violations, _ = _evaluate(mod, repo)
    assert len(violations) == 1
    assert violations[0].path == "crates/a.rs"
    assert violations[0].baseline == 50


# The ratchet's whole reason for existing: 13 files are already over, and a
# plain threshold would block every PR that touches them.
def test_an_existing_offender_may_be_edited_without_growing(mod, repo):
    _write(repo, "crates/big.rs", 300)
    _commit(repo, "base is already over")
    _git(repo, "checkout", "-q", "-b", "topic")
    # Same size, different content — the one-line fix case that used to be
    # blocked by the hook.
    (repo / "crates" / "big.rs").write_text(
        "\n".join(f"// edited {i}" for i in range(300)) + "\n", encoding="utf-8"
    )
    _commit(repo, "edit without growing")

    violations, checked = _evaluate(mod, repo)
    assert violations == [], "editing an over-ceiling file must stay allowed"
    assert checked == 1


def test_an_existing_offender_may_shrink(mod, repo):
    _write(repo, "crates/big.rs", 300)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/big.rs", 250)
    _commit(repo, "shrink")

    violations, _ = _evaluate(mod, repo)
    assert violations == [], "shrinking must always be allowed"


def test_an_existing_offender_may_not_grow(mod, repo):
    _write(repo, "crates/big.rs", 300)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/big.rs", 301)
    _commit(repo, "one more line")

    violations, _ = _evaluate(mod, repo)
    assert len(violations) == 1
    assert violations[0].baseline == 300
    assert violations[0].lines == 301


def test_a_new_oversized_file_fails_with_a_new_file_message(mod, repo):
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/fresh.rs", 400)
    _commit(repo, "add an oversized file")

    violations, _ = _evaluate(mod, repo)
    assert len(violations) == 1
    assert violations[0].baseline is None
    assert "new file" in violations[0].describe(100)


# A split deletes the original path. If deletions counted, the check would
# block the exact refactor it exists to encourage.
def test_deleting_an_oversized_file_is_not_a_violation(mod, repo):
    _write(repo, "crates/big.rs", 300)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    (repo / "crates" / "big.rs").unlink()
    _write(repo, "crates/part_a.rs", 80)
    _write(repo, "crates/part_b.rs", 80)
    _commit(repo, "split it up")

    violations, _ = _evaluate(mod, repo)
    assert violations == [], "a split must not be blocked by the ratchet"


def test_non_rust_and_out_of_scope_paths_are_ignored(mod, repo):
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    (repo / "notes.md").write_text("x\n" * 500, encoding="utf-8")
    _write(repo, "vendor/huge.rs", 900)
    _commit(repo, "unrelated bulk")

    violations, checked = _evaluate(mod, repo)
    assert violations == []
    assert checked == 0, "only .rs under the configured roots is checked"


def test_mod_rs_files_are_exempt(mod, repo):
    _write(repo, "crates/component/mod.rs", 300)
    _write(repo, "crates/component/implementation.rs", 300)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/component/mod.rs", 400)
    _write(repo, "crates/component/implementation.rs", 301)
    _commit(repo, "grow module surface and implementation")

    violations, checked = _evaluate(mod, repo)
    assert [violation.path for violation in violations] == [
        "crates/component/implementation.rs"
    ]
    assert checked == 1, "mod.rs files must be excluded from the ratchet"


def test_new_oversized_mod_rs_is_exempt(mod, repo):
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/new_component/mod.rs", 400)
    _commit(repo, "add module surface")

    violations, checked = _evaluate(mod, repo)
    assert violations == []
    assert checked == 0


def test_untouched_offenders_are_not_reported(mod, repo):
    """The cost lands on the change that causes it, not on every PR."""
    _write(repo, "crates/big.rs", 300)
    _write(repo, "crates/small.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/small.rs", 20)
    _commit(repo, "touch only the small file")

    violations, checked = _evaluate(mod, repo)
    assert violations == []
    assert checked == 1, "the untouched oversized file must not be examined"


def test_main_exits_nonzero_on_violation(mod, repo, monkeypatch, capsys):
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/a.rs", 400)
    _commit(repo, "grow")

    monkeypatch.chdir(repo)
    code = mod.main(["--base-ref", "main", "--ceiling", "100"])
    assert code == 1
    assert "loc_ratchet: FAIL" in capsys.readouterr().err


def test_an_unreachable_base_skips_rather_than_failing(mod, repo, monkeypatch, capsys):
    """A shallow CI checkout must not manufacture a violation it cannot prove.

    If this failed instead of skipping, honest PRs would go red and the first
    fix anyone reached for would be deleting the check.
    """
    _write(repo, "crates/a.rs", 10)
    _commit(repo, "base")

    monkeypatch.chdir(repo)
    code = mod.main(["--base-ref", "origin/does-not-exist", "--ceiling", "100"])
    assert code == 0
    assert "skipped" in capsys.readouterr().err


def test_base_sha_needs_no_shared_ancestry(mod, repo):
    """The CI case: compare two commits with no merge base at all.

    The first version of this check used `git merge-base`, which a shallow
    `pull_request` checkout cannot resolve -- so it skipped on every PR and
    enforced nothing. It reported `success` while doing no work, which is why
    this is pinned. Comparing against the base commit directly works because
    `git diff A B` compares trees, not history.
    """
    import os

    _write(repo, "crates/big.rs", 300)
    _commit(repo, "base")
    base_sha = _git(repo, "rev-parse", "HEAD").strip()

    # An orphan branch shares no history with `base_sha` whatsoever.
    _git(repo, "checkout", "-q", "--orphan", "detached")
    _write(repo, "crates/big.rs", 320)
    _commit(repo, "unrelated root")

    cwd = Path.cwd()
    os.chdir(repo)
    try:
        with pytest.raises(mod.NoMergeBase):
            mod._merge_base(base_sha)  # the mode that silently skipped in CI
        violations, _ = mod.evaluate("unused", ("crates",), 100, base_sha)
    finally:
        os.chdir(cwd)

    assert len(violations) == 1, "the ratchet must still see the growth"
    assert violations[0].baseline == 300
    assert violations[0].lines == 320


def test_a_stale_branch_is_not_blamed_for_changes_on_main(mod, repo):
    """Compare against the merge base, never the base tip.

    A branch behind main differs from the tip in files it never touched. An
    early version of this check compared against the tip and flagged
    `lifecycle/mod.rs` on a PR that did not touch it -- main had *shrunk* the
    file after the branch point, so the branch's untouched copy looked like
    growth. That would fail honest PRs for someone else's change.
    """
    import os

    _write(repo, "crates/other.rs", 300)
    _commit(repo, "base: other.rs is already over")
    _git(repo, "checkout", "-q", "-b", "topic")
    _write(repo, "crates/mine.rs", 10)
    _commit(repo, "topic touches only mine.rs")

    # main moves on, growing a file the branch never touched.
    _git(repo, "checkout", "-q", "main")
    _write(repo, "crates/other.rs", 500)
    _commit(repo, "main grows other.rs")
    _git(repo, "checkout", "-q", "topic")

    cwd = Path.cwd()
    os.chdir(repo)
    try:
        # merge-base mode: correct -- only the branch's own change is in scope.
        violations, checked = mod.evaluate("main", ("crates",), 100)
    finally:
        os.chdir(cwd)

    assert violations == [], (
        "the branch must not be blamed for main's change to a file it never "
        f"touched, got: {[v.path for v in violations]}"
    )
    assert checked == 1, "only the branch's own file should be examined"
