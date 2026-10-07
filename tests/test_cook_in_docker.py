"""Volume-naming guards for bench/cook_in_docker.sh.

The script used machine-wide volume names, so sibling checkouts (soldr,
soldr2, soldr3) shared them. Its per-run
``docker volume rm --force cook-soldr-home`` would then destroy the harness
volume out from under a run in another checkout, and all roots fought over a
single cargo target across different branches.

``SOLDR_COOK_PRINT_PLAN=1`` resolves the names and exits before touching
Docker, so these run anywhere bash exists.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / "bench" / "cook_in_docker.sh"


@dataclass(frozen=True)
class CookPlan:
    source_root: str
    worktree_root: str
    harness_volume: str
    target_volume: str
    cargo_volume: str


def find_bash() -> str | None:
    """Locate a POSIX bash without choosing the Windows WSL launcher.

    On Windows `shutil.which("bash")` usually resolves to the System32 WSL
    launcher, which is not a POSIX shell here.
    """
    candidates = [
        os.environ.get("SOLDR_TEST_BASH"),
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
        shutil.which("bash"),
    ]
    for candidate in candidates:
        if not candidate or not Path(candidate).exists():
            continue
        if os.name == "nt" and "system32" in str(Path(candidate).parent).lower():
            continue
        return candidate
    return None


BASH = find_bash()

pytestmark = pytest.mark.skipif(
    BASH is None, reason="a POSIX bash is required to evaluate the harness script"
)


def plan_for(root: Path) -> CookPlan:
    """Copy the script under `root` and return its resolved names."""
    bench = root / "bench"
    bench.mkdir(parents=True, exist_ok=True)
    copied = bench / SCRIPT.name
    copied.write_bytes(SCRIPT.read_bytes())

    # as_posix(): a Windows backslash path does not survive into bash, which
    # would fail the `cd "$(dirname ...)"` that resolves REPO_ROOT.
    assert BASH is not None
    stdout_path = root / "plan.stdout"
    stderr_path = root / "plan.stderr"
    with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
        result = subprocess.run(
            [BASH, copied.as_posix()],
            stdout=stdout,
            stderr=stderr,
            check=False,
            env={"SOLDR_COOK_PRINT_PLAN": "1", "PATH": os.environ.get("PATH", "")},
        )
    assert result.returncode == 0, (
        f"script failed ({result.returncode}):\n{stderr_path.read_text()}"
    )
    values = {}
    for line in stdout_path.read_text().splitlines():
        key, _, value = line.partition("=")
        values[key] = value
    return CookPlan(**values)


def test_sibling_checkouts_get_distinct_volumes(tmp_path: Path) -> None:
    plans = {name: plan_for(tmp_path / name) for name in ("soldr", "soldr2", "soldr3")}

    for key in ("harness_volume", "target_volume", "cargo_volume"):
        names = [getattr(plan, key) for plan in plans.values()]
        assert len(set(names)) == len(names), f"{key} collided across roots: {names}"

    # The harness volume is force-removed every run; a collision there would
    # destroy another checkout's in-flight state.
    assert plans["soldr"].harness_volume != plans["soldr2"].harness_volume


def test_volume_names_are_docker_safe_and_readable(tmp_path: Path) -> None:
    plan = plan_for(tmp_path / "soldr2")

    for key in ("harness_volume", "target_volume", "cargo_volume"):
        name = getattr(plan, key)
        assert name[0].isalnum(), name
        assert all(char.isalnum() or char in "_.-" for char in name), name
        assert "soldr2" in name, f"{key} should carry the leaf name: {name}"


def test_plan_is_deterministic(tmp_path: Path) -> None:
    root = tmp_path / "soldr2"
    assert plan_for(root) == plan_for(root)


def test_directory_names_docker_would_reject_are_sanitized(tmp_path: Path) -> None:
    plan = plan_for(tmp_path / "soldr wt #1735")

    name = plan.target_volume
    assert name.startswith("soldr-perf-target-"), name
    assert all(char.isalnum() or char in "_.-" for char in name), name


def test_linked_worktree_shares_warm_volumes_but_not_harness(tmp_path: Path) -> None:
    root = tmp_path / "soldr"
    root.mkdir()
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
        check=True,
    )
    worktree = tmp_path / "linked-worktree"
    subprocess.run(
        [
            "git",
            "-C",
            str(root),
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            str(worktree),
        ],
        check=True,
    )

    root_plan = plan_for(root)
    worktree_plan = plan_for(worktree)
    assert worktree_plan.source_root == root_plan.source_root
    assert worktree_plan.worktree_root != root_plan.worktree_root
    assert worktree_plan.harness_volume != root_plan.harness_volume
    for key in ("target_volume", "cargo_volume"):
        assert getattr(worktree_plan, key) == getattr(root_plan, key)
