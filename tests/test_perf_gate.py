"""The perf/benchmark gate decides whether expensive runs fire at all.

`.github/scripts/perf_gate.py` gates two workflows (perf-matrix and
benchmark-stats, ~30-60 min a piece) and had no tests. Both ways it can be
wrong are silent:

* stuck OFF -- perf regressions stop being measured and nobody is told;
* stuck ON  -- every push to main burns an expensive runner, which is the
  exact compute waste soldr#1978 exists to remove.

The decision is a small matrix -- mode x "was there recent non-bot activity"
-- and the two modes deliberately *invert* the same signal, so a single
flipped branch is easy to introduce and impossible to notice. These tests pin
all six outcomes, plus the bot classifier the signal depends on.
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / ".github" / "scripts" / "perf_gate.py"


@pytest.fixture(scope="module")
def gate():
    spec = importlib.util.spec_from_file_location("perf_gate", SCRIPT)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["perf_gate"] = module
    spec.loader.exec_module(module)
    return module


# --- the bot classifier the whole signal rests on -------------------------


@pytest.mark.parametrize(
    "email",
    [
        "github-actions[bot]@users.noreply.github.com",
        "actions@github.com",
        "noreply@github.com",
        "renovate[bot]@users.noreply.github.com",
        "dependabot[bot]@users.noreply.github.com",
        "soldr-release-bot@example.com",
        "  actions@github.com  ",  # surrounding whitespace must not defeat it
        "ACTIONS@GITHUB.COM",  # documented case-insensitive
    ],
)
def test_bot_identities_are_recognized(gate, email):
    assert gate.is_bot(email) is True, email


@pytest.mark.parametrize(
    "email",
    [
        "someone@example.com",
        "zach.vorhies@gmail.com",
        # The subtle one, called out in the script's own docstring: Claude's
        # co-author trailer. `git log --format=%ae` reports the AUTHOR, so a
        # human-authored commit with this trailer must still count as human
        # activity -- classifying it as a bot would make busy days look quiet
        # and fire perf runs during exactly the noise they avoid.
        "noreply@anthropic.com",
    ],
)
def test_human_identities_are_not_bots(gate, email):
    assert gate.is_bot(email) is False, email


# --- the decision matrix --------------------------------------------------


def _git(repo: Path, *args: str, env: "dict[str, str] | None" = None) -> str:
    import os

    full_env = {**os.environ, **(env or {})}
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
        env=full_env,
    ).stdout


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    r = tmp_path / "repo"
    r.mkdir()
    _git(r, "init", "-q", "-b", "main")
    _git(r, "config", "user.email", "t@example.com")
    _git(r, "config", "user.name", "t")
    return r


def _commit(repo: Path, email: str, hours_ago: int, message: str) -> None:
    """Commit authored `hours_ago` hours in the past by `email`."""
    import time

    when = int(time.time()) - hours_ago * 3600
    (repo / "f.txt").write_text(message, encoding="utf-8")
    _git(repo, "add", "-A")
    _git(
        repo,
        "commit",
        "-q",
        "-m",
        message,
        env={
            "GIT_AUTHOR_EMAIL": email,
            "GIT_COMMITTER_EMAIL": email,
            "GIT_AUTHOR_DATE": f"{when} +0000",
            "GIT_COMMITTER_DATE": f"{when} +0000",
        },
    )


def _run(
    gate, repo: Path, mode: str, tmp_path: Path, hours: int = 24
) -> "tuple[bool, str]":
    """Invoke `main()` against `repo` and read the decision back out of the
    emitted `$GITHUB_OUTPUT`, which is how the workflows consume it."""
    import os

    out = tmp_path / f"gh-output-{mode}.txt"
    out.write_text("", encoding="utf-8")
    argv = sys.argv[:]
    cwd = Path.cwd()
    os.environ["GITHUB_OUTPUT"] = str(out)
    sys.argv = ["perf_gate.py", "--mode", mode, "--hours", str(hours)]
    os.chdir(repo)
    try:
        assert gate.main() == 0, "the gate must always exit 0"
    finally:
        os.chdir(cwd)
        sys.argv = argv
        os.environ.pop("GITHUB_OUTPUT", None)

    text = out.read_text(encoding="utf-8")
    should_run = "should_run=true" in text
    reason = next(
        (
            line.split("=", 1)[1]
            for line in text.splitlines()
            if line.startswith("reason=")
        ),
        "",
    )
    return should_run, reason


def test_push_skips_during_an_active_burst(gate, repo, tmp_path):
    # A busy day already produces benchmark noise, so per-merge runs are
    # suppressed and the nightly cron captures the cumulative trend.
    _commit(repo, "human@example.com", hours_ago=2, message="recent human work")
    should_run, reason = _run(gate, repo, "push", tmp_path)
    assert should_run is False, reason
    assert "skipping perf run" in reason, reason


def test_push_runs_after_a_quiet_window(gate, repo, tmp_path):
    _commit(repo, "human@example.com", hours_ago=100, message="old human work")
    should_run, reason = _run(gate, repo, "push", tmp_path)
    assert should_run is True, reason


def test_schedule_runs_when_the_day_had_activity(gate, repo, tmp_path):
    _commit(repo, "human@example.com", hours_ago=2, message="recent human work")
    should_run, reason = _run(gate, repo, "schedule", tmp_path)
    assert should_run is True, reason


def test_schedule_skips_a_quiet_day(gate, repo, tmp_path):
    _commit(repo, "human@example.com", hours_ago=100, message="old human work")
    should_run, reason = _run(gate, repo, "schedule", tmp_path)
    assert should_run is False, reason


def test_the_two_modes_invert_the_same_signal(gate, repo, tmp_path):
    # The property that makes a flipped branch dangerous: for identical
    # history the modes must disagree. Asserting it directly means a change
    # that accidentally aligns them fails here even if each mode's own test
    # were updated to match the new behaviour.
    _commit(repo, "human@example.com", hours_ago=2, message="recent human work")
    push, _ = _run(gate, repo, "push", tmp_path)
    sched, _ = _run(gate, repo, "schedule", tmp_path)
    assert push != sched, "push and schedule must invert the same activity signal"


def test_bot_commits_do_not_count_as_activity(gate, repo, tmp_path):
    # A fresh bot commit on top of old human work must not read as a busy
    # day -- otherwise release automation pushing to main would suppress
    # every subsequent perf run.
    _commit(repo, "human@example.com", hours_ago=100, message="old human work")
    _commit(repo, "github-actions[bot]@users.noreply.github.com", 0, "bot bump")
    should_run, reason = _run(gate, repo, "push", tmp_path)
    assert should_run is True, reason
    assert "human@example.com" in reason, reason


def test_no_human_commits_at_all_biases_each_mode_safely(gate, repo, tmp_path):
    # No signal either way. Push mode calls that quiet enough to run;
    # schedule mode has nothing new to benchmark and skips.
    _commit(repo, "github-actions[bot]@users.noreply.github.com", 1, "bot only")
    push, push_reason = _run(gate, repo, "push", tmp_path)
    sched, sched_reason = _run(gate, repo, "schedule", tmp_path)
    assert push is True, push_reason
    assert sched is False, sched_reason


def test_emit_output_is_a_noop_without_github_output(gate, capsys):
    # Running the script locally (no $GITHUB_OUTPUT) must print the reason
    # and not raise -- the gate is also a human-readable diagnostic.
    import os

    os.environ.pop("GITHUB_OUTPUT", None)
    gate.emit_output(True, "some reason")
    assert "some reason" in capsys.readouterr().out
