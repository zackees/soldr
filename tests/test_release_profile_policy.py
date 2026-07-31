"""Tests for the `--release` CI policy check (soldr#1981)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "verify_release_profile_policy.py"
)


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "verify_release_profile_policy")


def _write(dir_: Path, name: str, body: str) -> None:
    (dir_ / name).write_text(body, encoding="utf-8")


def test_a_cheap_profile_passes(mod, tmp_path):
    _write(
        tmp_path,
        "cheap.yml",
        "jobs:\n  a:\n    run: cargo build --profile ci-release\n",
    )
    assert mod.scan(tmp_path) == []


def test_release_in_a_non_allowlisted_workflow_fails(mod, tmp_path):
    _write(
        tmp_path,
        "greedy.yml",
        "jobs:\n  a:\n    run: cargo build --release -p soldr-cli\n",
    )
    findings = mod.scan(tmp_path)
    assert len(findings) == 1
    name, number, line = findings[0]
    assert name == "greedy.yml"
    assert number == 3
    assert "--release" in line


def test_an_allowlisted_workflow_is_skipped(mod, tmp_path):
    name = next(iter(mod.ALLOWLIST))
    _write(tmp_path, name, "jobs:\n  a:\n    run: cargo build --release\n")
    assert mod.scan(tmp_path) == []


# `_ci-cross-build-linux.yml` carries a comment recording that Stage B moved
# *off* `--release`. A naive scan flags exactly that line, which would make the
# check punish the very cleanup it exists to protect.
def test_a_comment_about_release_is_not_a_violation(mod, tmp_path):
    _write(
        tmp_path,
        "documented.yml",
        "jobs:\n  a:\n    # after Stage B switched from --release to --profile ci-release:\n"
        "    run: cargo build --profile ci-release\n",
    )
    assert mod.scan(tmp_path) == []


# `--release-notes` and similar must not trip the word match.
def test_a_longer_flag_containing_release_is_not_a_violation(mod, tmp_path):
    _write(
        tmp_path,
        "other.yml",
        "jobs:\n  a:\n    run: gh release create --release-notes x\n",
    )
    assert mod.scan(tmp_path) == []


def test_release_before_a_line_continuation_is_caught(mod, tmp_path):
    _write(
        tmp_path,
        "wrapped.yml",
        "jobs:\n  a:\n    run: |\n      cargo build \\\n        --release \\\n        --locked\n",
    )
    findings = mod.scan(tmp_path)
    assert len(findings) == 1, f"a wrapped invocation must still be caught: {findings}"


# An exemption that outlives its reason silently re-permits what the policy
# forbids. soldr#1982 deleted a workflow outright, which is how this happens.
def test_an_allowlist_entry_for_a_missing_workflow_is_reported(mod, tmp_path):
    stale = mod.unused_allowlist_entries(tmp_path)
    assert stale, "every allowlisted name is absent here, so all should be stale"
    assert all("no such workflow" in entry for entry in stale)


def test_an_allowlist_entry_that_no_longer_uses_release_is_reported(mod, tmp_path):
    name = next(iter(mod.ALLOWLIST))
    _write(tmp_path, name, "jobs:\n  a:\n    run: cargo build --profile ci-release\n")
    stale = mod.unused_allowlist_entries(tmp_path)
    assert f"{name} (no longer uses --release)" in stale


def test_every_allowlist_entry_carries_a_reason(mod):
    """The allowlist is reason-bearing by design; a bare name teaches nothing."""
    for name, reason in mod.ALLOWLIST.items():
        assert reason.strip(), f"{name} needs a reason"
        assert (
            len(reason) > 20
        ), f"{name}'s reason is too terse to be useful: {reason!r}"


def test_the_real_repository_is_clean(mod):
    """The policy must hold on the tree that ships it."""
    workflows = Path(__file__).resolve().parents[1] / ".github" / "workflows"
    if not workflows.is_dir():
        pytest.skip("workflows directory absent")
    assert mod.scan(workflows) == []
    assert mod.unused_allowlist_entries(workflows) == []
