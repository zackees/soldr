"""The local gate's wiring stays consistent (zackees/ci.yml#166, #168).

`ci-lint local-gate lint` (a lint-lane check) proves the workflow side over
the network; these offline asserts keep the pieces that ci-lint cannot see
in step: the one ci_lint ref named in three places, and the isolation
guard's actual behaviour.
"""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import load_script_module

ROOT = Path(__file__).resolve().parent.parent
GATE = load_script_module(ROOT / "ci" / "local_gate.py", "soldr_local_gate_wiring")
WRAPPER = ROOT / ".github" / "scripts" / "nextest_wrapper.sh"


def test_one_ci_lint_ref_everywhere() -> None:
    ref = GATE.CI_LINT_REF
    assert re.fullmatch(r"[0-9a-f]{40}", ref)
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert f"ref: {ref}" in workflow
    assert ref in (ROOT / "local-gate.toml").read_text(encoding="utf-8")
    pinned = set(re.findall(r"zackees/ci\.yml@([0-9a-f]{40})", workflow))
    assert pinned <= {ref}


def test_lint_job_runs_only_the_lint_lane() -> None:
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    assert gate["mirrors"] == ["ci.yml:lint"]
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert (
        "run: uv run --no-project --python 3.13 python ci/local_gate.py --lane lint"
        in workflow
    )


def test_attested_skip_is_declared_and_wired() -> None:
    """zackees/ci.yml#190 (GATE-008): the skip jobs consume ci-mode's
    `trusted` output, the protected `Lint` status still reports through
    lint-docs, and pushes to main always run (verify never trusts them)."""
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    trust = gate["trust"]
    assert trust["mode"] == "enforce"
    assert trust["skip"] == ["ci.yml:lint", "ci.yml:build-linux-x64"]
    assert trust.get("audit-rate", 10) >= 2
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "local-gate verify --repo . --trust --github-output" in workflow
    assert "trusted: ${{ steps.gate.outputs.trusted }}" in workflow
    assert workflow.count("needs.ci-mode.outputs.trusted != 'true'") == 2
    assert "needs.ci-mode.outputs.trusted == 'true'" in workflow  # lint-docs
    assert "\n    branches:\n      - main\n" in workflow


def test_the_test_suite_only_runs_isolated_locally(monkeypatch) -> None:
    monkeypatch.delenv("SOLDR_LOCAL_GATE_BOSN", raising=False)
    lanes = {check.name: check for check in GATE.checks()}
    tests = lanes["soldr tests (isolated, bosn)"]
    assert tests.argv == ("bosn", "run", "--task", "test")
    for check in GATE.checks():
        if check.lane != "tests":
            assert "nextest" not in check.argv and "ci-test" not in check.argv, check


def test_source_bosn_override_keeps_isolation_and_version_gate(monkeypatch) -> None:
    monkeypatch.setenv("SOLDR_LOCAL_GATE_BOSN", "/owned/bosn-native")
    check = next(check for check in GATE.checks() if check.lane == "tests")
    assert check.argv == ("/owned/bosn-native", "run", "--task", "test")
    assert check.min_version == (0, 1, 6)
    assert check.exclusive


@dataclass(frozen=True)
class WrapperRun:
    returncode: int
    stderr: str
    stdout: str


def _wrapper(**extra: str) -> WrapperRun:
    """Run the nextest wrapper with stderr captured through a file, not a
    pipe (zackees/ci.yml PY-003)."""
    env = {
        k: v for k, v in os.environ.items() if k not in ("CI", "SOLDR_TEST_ISOLATED")
    }
    env.update(extra)
    env["SOLDR_NEXTEST_NATIVE_WRAPPER"] = "/bin/true"
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        proc = subprocess.run(
            ["sh", str(WRAPPER), "true"],
            env=env,
            stdout=out,
            stderr=err,
            check=False,
        )
        out.seek(0)
        err.seek(0)
        return WrapperRun(
            proc.returncode,
            err.read().decode("utf-8", errors="replace"),
            out.read().decode("utf-8", errors="replace"),
        )


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_isolation_guard_refuses_a_developer_host() -> None:
    refused = _wrapper()
    assert refused.returncode != 0
    assert "bosn run --task test" in refused.stderr
    assert _wrapper(CI="true").returncode == 0
    assert _wrapper(SOLDR_TEST_ISOLATED="1").returncode == 0


def test_isolated_suite_requires_a_bosn_that_does_not_reap_bursts() -> None:
    # zackees/bosn#317: bosn < 0.1.5 reaped `ci-test` on an output burst.
    tests = {check.name: check for check in GATE.checks()}[
        "soldr tests (isolated, bosn)"
    ]
    assert tests.min_version is not None and tests.min_version >= (0, 1, 6)
