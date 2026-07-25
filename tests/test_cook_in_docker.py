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
from pathlib import Path

import pytest

SCRIPT = Path(__file__).parents[1] / "bench" / "cook_in_docker.sh"


def find_bash() -> str | None:
    """Locate a POSIX bash, proving it works before returning it.

    On Windows `shutil.which("bash")` usually resolves to the System32 WSL
    launcher, which is not a POSIX shell here and fails with a UTF-16 Windows
    error string. Each candidate is executed and validated instead of trusted.
    """
    candidates = [
        os.environ.get("SOLDR_TEST_BASH"),
        shutil.which("bash"),
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files\Git\usr\bin\bash.exe",
    ]
    for candidate in candidates:
        if not candidate or not Path(candidate).exists():
            continue
        try:
            probe = subprocess.run(
                [candidate, "-c", "printf ok"], capture_output=True, text=True, timeout=30
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if probe.returncode == 0 and probe.stdout.strip() == "ok":
            return candidate
    return None


BASH = find_bash()

pytestmark = pytest.mark.skipif(
    BASH is None, reason="a POSIX bash is required to evaluate the harness script"
)


def plan_for(root: Path) -> dict[str, str]:
    """Copy the script under `root` and return its resolved names."""
    bench = root / "bench"
    bench.mkdir(parents=True, exist_ok=True)
    copied = bench / SCRIPT.name
    copied.write_bytes(SCRIPT.read_bytes())

    # as_posix(): a Windows backslash path does not survive into bash, which
    # would fail the `cd "$(dirname ...)"` that resolves REPO_ROOT.
    assert BASH is not None
    out = subprocess.run(
        [BASH, copied.as_posix()],
        capture_output=True,
        text=True,
        check=False,
        env={"SOLDR_COOK_PRINT_PLAN": "1", "PATH": os.environ.get("PATH", "")},
    )
    assert out.returncode == 0, f"script failed ({out.returncode}):\n{out.stderr}"
    plan = {}
    for line in out.stdout.splitlines():
        key, _, value = line.partition("=")
        plan[key] = value
    return plan


def test_sibling_checkouts_get_distinct_volumes(tmp_path: Path) -> None:
    plans = {name: plan_for(tmp_path / name) for name in ("soldr", "soldr2", "soldr3")}

    for key in ("harness_volume", "target_volume", "cargo_volume"):
        names = [plan[key] for plan in plans.values()]
        assert len(set(names)) == len(names), f"{key} collided across roots: {names}"

    # The harness volume is force-removed every run; a collision there would
    # destroy another checkout's in-flight state.
    assert plans["soldr"]["harness_volume"] != plans["soldr2"]["harness_volume"]


def test_volume_names_are_docker_safe_and_readable(tmp_path: Path) -> None:
    plan = plan_for(tmp_path / "soldr2")

    for key in ("harness_volume", "target_volume", "cargo_volume"):
        name = plan[key]
        assert name[0].isalnum(), name
        assert all(char.isalnum() or char in "_.-" for char in name), name
        assert "soldr2" in name, f"{key} should carry the leaf name: {name}"


def test_plan_is_deterministic(tmp_path: Path) -> None:
    root = tmp_path / "soldr2"
    assert plan_for(root) == plan_for(root)


def test_directory_names_docker_would_reject_are_sanitized(tmp_path: Path) -> None:
    plan = plan_for(tmp_path / "soldr wt #1735")

    name = plan["target_volume"]
    assert name.startswith("soldr-perf-target-"), name
    assert all(char.isalnum() or char in "_.-" for char in name), name
