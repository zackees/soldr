"""Tests for the vendor-state gate.

`verify_vendor_state.py` enforces the `docs/VENDORING.md` contract on every
PR touching `_vender/`. It had no tests, and it is currently *dormant*: the
repo is in submodule mode, so the script short-circuits before checks 2 and 3
ever run. Dormant code that a future flat-vendor switch would suddenly rely on
is exactly the code most likely to be quietly broken -- so these tests drive
the flat-vendor path directly rather than only the mode the repo happens to
be in today.

The regression that motivated them is `test_dep_detection_survives_reformatting`:
the detector used to require the literal bytes `path = "_vender/`, so any
re-spacing of the dep made it report **"all vendor-state checks pass"** with
every check skipped.
"""

from __future__ import annotations

import datetime as dt
import subprocess
import sys
import time
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "verify_vendor_state.py"


@pytest.fixture(scope="module")
def vvs():
    return load_script_module(SCRIPT, "verify_vendor_state")


def _cargo(tmp_path: Path, body: str) -> Path:
    path = tmp_path / "Cargo.toml"
    path.write_text(body, encoding="utf-8")
    return path


# --- check 1: is the vendored dep even detected? --------------------------


@pytest.mark.parametrize(
    "body",
    [
        # The canonical form in the repo today.
        '[dependencies]\nzccache = { version = "1.12.17", '
        'path = "../../_vender/zccache/crates/zccache", '
        'default-features = false, features = ["cli"] }\n',
        # Re-spaced variants. Every one of these used to return False and
        # silently disable the gate.
        '[dependencies]\nzccache = { path="../../_vender/zccache/crates/zccache" }\n',
        '[dependencies]\nzccache = { path =  "../../_vender/zccache/crates/zccache" }\n',
        '[dependencies]\nzccache = { path\t= "../../_vender/zccache/crates/zccache" }\n',
        # The separate-table form is equally valid TOML.
        '[dependencies.zccache]\npath = "../../_vender/zccache/crates/zccache"\n',
        # Platform-gated deps live in a nested table.
        '[target."cfg(unix)".dependencies]\n'
        'zccache = { path = "../../_vender/zccache/crates/zccache" }\n',
        # A crate at a different depth uses a different number of `..`.
        '[dependencies]\nzccache = { path = "_vender/zccache/crates/zccache" }\n',
        '[dev-dependencies]\nzccache = { path = "../../../_vender/zccache" }\n',
    ],
)
def test_dep_detection_survives_reformatting(vvs, tmp_path, body):
    assert vvs.soldr_cargo_uses_vendor(_cargo(tmp_path, body)) is True


@pytest.mark.parametrize(
    "body",
    [
        # A released pin is the whole point of ending a vendor -- it must
        # NOT trip the gate.
        '[dependencies]\nzccache = { version = "1.12.17", '
        'git = "https://github.com/zackees/zccache" }\n',
        '[dependencies]\nzccache = "1.12.17"\n',
        # A path dep that isn't the vendor tree.
        '[dependencies]\nzccache = { path = "../zccache-local" }\n',
        # No zccache dep at all.
        '[dependencies]\nserde = "1"\n',
    ],
)
def test_non_vendored_deps_do_not_trip_the_gate(vvs, tmp_path, body):
    assert vvs.soldr_cargo_uses_vendor(_cargo(tmp_path, body)) is False


def test_missing_cargo_toml_is_not_vendored(vvs, tmp_path):
    assert vvs.soldr_cargo_uses_vendor(tmp_path / "nope.toml") is False


def test_unparseable_cargo_toml_is_loud_not_silent(vvs, tmp_path):
    # Every default is a wrong answer here: False disables the gate, True
    # fails unrelated PRs. So it must raise and become exit 2.
    bad = _cargo(tmp_path, "[dependencies\nzccache = {")
    with pytest.raises(vvs.VendorStateError):
        vvs.soldr_cargo_uses_vendor(bad)


def test_vendor_without_state_file_is_an_error(vvs, tmp_path):
    cargo = _cargo(
        tmp_path,
        '[dependencies]\nzccache = { path = "../../_vender/zccache" }\n',
    )
    errors = vvs.check_vendor_active_means_state_exists(
        cargo, tmp_path / ".vendor-state", tmp_path
    )
    assert len(errors) == 1
    assert "Check 1 failed" in errors[0]


def test_submodule_mode_needs_no_vendor_state(vvs, tmp_path):
    # A submodule tracks upstream by commit pointer, so the .vendor-state
    # drift contract doesn't apply -- this is the repo's current mode.
    cargo = _cargo(
        tmp_path,
        '[dependencies]\nzccache = { path = "../../_vender/zccache" }\n',
    )
    (tmp_path / ".gitmodules").write_text(
        '[submodule "_vender/zccache"]\n'
        "\tpath = _vender/zccache\n"
        "\turl = https://github.com/zackees/zccache.git\n",
        encoding="utf-8",
    )
    assert vvs.vendor_is_git_submodule(tmp_path) is True
    assert (
        vvs.check_vendor_active_means_state_exists(
            cargo, tmp_path / ".vendor-state", tmp_path
        )
        == []
    )


def test_gitmodules_for_another_submodule_is_not_the_vendor(vvs, tmp_path):
    (tmp_path / ".gitmodules").write_text(
        '[submodule "third_party/other"]\n\tpath = third_party/other\n',
        encoding="utf-8",
    )
    assert vvs.vendor_is_git_submodule(tmp_path) is False


# --- check 2: the deadline ------------------------------------------------


def test_future_deadline_passes(vvs, tmp_path):
    future = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=3)
    assert (
        vvs.check_deadline_in_future({"deadline": future.isoformat()}, tmp_path) == []
    )


def test_past_deadline_fails(vvs, tmp_path):
    past = dt.datetime.now(dt.timezone.utc) - dt.timedelta(days=1)
    errors = vvs.check_deadline_in_future({"deadline": past.isoformat()}, tmp_path)
    assert len(errors) == 1
    assert "Check 2 failed" in errors[0]
    assert "in the past" in errors[0]


def test_deadline_accepts_a_native_toml_datetime(vvs, tmp_path):
    # TOML has a real datetime type, so tomllib hands back a datetime
    # object rather than a string -- and a naive one if the author omitted
    # the offset. Both must be handled, or a correct .vendor-state file
    # crashes the gate.
    naive = dt.datetime.now() + dt.timedelta(days=3)
    assert vvs.check_deadline_in_future({"deadline": naive}, tmp_path) == []


def test_missing_deadline_fails(vvs, tmp_path):
    errors = vvs.check_deadline_in_future({}, tmp_path)
    assert len(errors) == 1
    assert "missing the `deadline` field" in errors[0]


def test_unparseable_deadline_fails(vvs, tmp_path):
    errors = vvs.check_deadline_in_future({"deadline": "next tuesday"}, tmp_path)
    assert len(errors) == 1
    assert "not a valid ISO-8601" in errors[0]


def test_z_suffixed_deadline_is_understood(vvs, tmp_path):
    future = dt.datetime.now(dt.timezone.utc) + dt.timedelta(days=3)
    stamp = future.strftime("%Y-%m-%dT%H:%M:%SZ")
    assert vvs.check_deadline_in_future({"deadline": stamp}, tmp_path) == []


# --- check 3: deltas must reach upstream within the grace window ----------


def _git(repo: Path, *args: str, env: "dict[str, str] | None" = None) -> str:
    import os

    return subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        text=True,
        env={**os.environ, **(env or {})},
    ).stdout.strip()


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    r = tmp_path / "repo"
    r.mkdir()
    _git(r, "init", "-q", "-b", "main")
    _git(r, "config", "user.email", "t@example.com")
    _git(r, "config", "user.name", "t")
    return r


def _commit_days_ago(repo: Path, days: float, message: str) -> str:
    when = int(time.time() - days * 86400)
    (repo / "f.txt").write_text(message, encoding="utf-8")
    _git(repo, "add", "-A")
    _git(
        repo,
        "commit",
        "-q",
        "-m",
        message,
        env={
            "GIT_AUTHOR_DATE": f"{when} +0000",
            "GIT_COMMITTER_DATE": f"{when} +0000",
        },
    )
    return _git(repo, "rev-parse", "HEAD")


def test_delta_with_upstream_pr_always_passes(vvs, repo):
    sha = _commit_days_ago(repo, 400, "ancient")
    state = {
        "deltas": [
            {"soldr_commit": sha, "upstream_pr": "https://github.com/x/y/pull/1"}
        ]
    }
    assert vvs.check_delta_pr_within_grace(state, repo / ".vendor-state", repo) == []


def test_recent_delta_without_upstream_pr_is_still_in_grace(vvs, repo):
    sha = _commit_days_ago(repo, 2, "fresh delta")
    state = {"deltas": [{"soldr_commit": sha, "summary": "fresh"}]}
    assert vvs.check_delta_pr_within_grace(state, repo / ".vendor-state", repo) == []


def test_stale_delta_without_upstream_pr_fails(vvs, repo):
    sha = _commit_days_ago(repo, 30, "stale delta")
    state = {"deltas": [{"soldr_commit": sha, "summary": "stale"}]}
    errors = vvs.check_delta_pr_within_grace(state, repo / ".vendor-state", repo)
    assert len(errors) == 1
    assert "Check 3 failed" in errors[0]
    assert "30." in errors[0]


def test_the_grace_boundary_is_seven_days(vvs, repo):
    # Pin the boundary itself: shortening or lengthening DELTA_GRACE_DAYS
    # changes who gets nagged and when, so it should not drift silently.
    assert vvs.DELTA_GRACE_DAYS == 7
    just_inside = _commit_days_ago(repo, 6.5, "inside")
    assert (
        vvs.check_delta_pr_within_grace(
            {"deltas": [{"soldr_commit": just_inside}]}, repo / ".vendor-state", repo
        )
        == []
    )
    just_outside = _commit_days_ago(repo, 7.5, "outside")
    assert (
        len(
            vvs.check_delta_pr_within_grace(
                {"deltas": [{"soldr_commit": just_outside}]},
                repo / ".vendor-state",
                repo,
            )
        )
        == 1
    )


def test_unknown_commit_fails_rather_than_passing(vvs, repo):
    # A shallow clone or rewritten history must not become a free pass --
    # unknown age is treated pessimistically.
    _commit_days_ago(repo, 1, "base")
    state = {"deltas": [{"soldr_commit": "0" * 40, "summary": "ghost"}]}
    errors = vvs.check_delta_pr_within_grace(state, repo / ".vendor-state", repo)
    assert len(errors) == 1
    assert "not present" in errors[0]


def test_delta_with_neither_commit_nor_pr_fails(vvs, repo):
    state = {"deltas": [{"summary": "untracked hack"}]}
    errors = vvs.check_delta_pr_within_grace(state, repo / ".vendor-state", repo)
    assert len(errors) == 1
    assert "untracked hack" in errors[0]


@pytest.mark.parametrize("deltas", ["not-a-list", 42])
def test_malformed_deltas_array_fails(vvs, repo, deltas):
    errors = vvs.check_delta_pr_within_grace(
        {"deltas": deltas}, repo / ".vendor-state", repo
    )
    assert len(errors) == 1
    assert "not an array" in errors[0]


def test_delta_entry_that_is_not_a_table_fails(vvs, repo):
    errors = vvs.check_delta_pr_within_grace(
        {"deltas": ["oops"]}, repo / ".vendor-state", repo
    )
    assert len(errors) == 1
    assert "not a table" in errors[0]


def test_no_deltas_is_fine(vvs, repo):
    assert vvs.check_delta_pr_within_grace({}, repo / ".vendor-state", repo) == []


# --- end to end -----------------------------------------------------------


def _run_main(vvs, repo_root: Path) -> int:
    argv = sys.argv[:]
    sys.argv = ["verify_vendor_state.py", "--repo-root", str(repo_root)]
    try:
        return vvs.main()
    finally:
        sys.argv = argv


def _write_cargo(repo_root: Path, body: str) -> None:
    crate = repo_root / "crates" / "soldr-cli"
    crate.mkdir(parents=True, exist_ok=True)
    (crate / "Cargo.toml").write_text(body, encoding="utf-8")


def test_main_passes_in_submodule_mode(vvs, tmp_path):
    _write_cargo(
        tmp_path,
        '[dependencies]\nzccache = { path = "../../_vender/zccache" }\n',
    )
    (tmp_path / ".gitmodules").write_text(
        "\tpath = _vender/zccache\n", encoding="utf-8"
    )
    assert _run_main(vvs, tmp_path) == 0


def test_main_fails_when_vendored_with_no_state_and_no_submodule(vvs, tmp_path):
    _write_cargo(
        tmp_path,
        '[dependencies]\nzccache = { path = "../../_vender/zccache" }\n',
    )
    assert _run_main(vvs, tmp_path) == 1


def test_main_reports_exit_2_on_unparseable_cargo_toml(vvs, tmp_path):
    # Not 0. The distinction matters: 1 is "you broke the contract",
    # 2 is "the gate could not evaluate the contract", and 0 would be a
    # lie either way.
    _write_cargo(tmp_path, "[dependencies\nzccache = {")
    assert _run_main(vvs, tmp_path) == 2


def test_main_passes_when_the_dep_is_a_released_pin(vvs, tmp_path):
    _write_cargo(tmp_path, '[dependencies]\nzccache = "1.12.17"\n')
    assert _run_main(vvs, tmp_path) == 0
