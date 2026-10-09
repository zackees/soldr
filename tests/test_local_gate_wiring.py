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
    # zackees/ci.yml#198 (GATE-010): each skip job consumes its own per-job
    # decision, and Linux x64 still runs when only Lint was skipped.
    assert "needs.ci-mode.outputs.skip_lint != 'true'" in workflow
    assert "needs.ci-mode.outputs.skip_build-linux-x64 != 'true'" in workflow
    assert (
        "(needs.lint.result == 'success' || needs.ci-mode.outputs.skip_lint == 'true')"
        in workflow
    )
    assert "needs.ci-mode.outputs.skip_lint == 'true'" in workflow  # lint-docs
    assert "\n    branches:\n      - main\n" in workflow


def test_ci_attestations_cover_every_skip_job() -> None:
    """zackees/ci.yml#198: every [gate.trust] skip job is mapped to gates,
    and every gate names a declared lane (ci-lint local-gate lint checks the
    same; this keeps it offline)."""
    gate = tomllib.loads((ROOT / "local-gate.toml").read_text(encoding="utf-8"))["gate"]
    text = (ROOT / "ci-attestations.yml").read_text(encoding="utf-8")
    for job in gate["trust"]["skip"]:
        assert f"  {job}:" in text, job
    for lane in re.findall(r"\{lane: ([a-z-]+)\}", text):
        assert lane in gate["lanes"], lane
    pre = (ROOT / ".github" / "workflows" / "ci-pre.yml").read_text(encoding="utf-8")
    assert "ci_lint attest keys" in pre
    assert pre.count("steps.att.outputs.stem_") == 16


def test_the_isolated_test_run_proves_its_tree() -> None:
    """zackees/ci.yml#196: the bosn test check carries a nonce the container
    must echo from its /repo, so a container bound to another worktree
    (zackees/bosn#314) fails the gate instead of attesting the wrong tree."""
    tests = next(c for c in GATE.checks() if c.lane == "tests")
    assert tests.tree_nonce
    task = (ROOT / "ci" / "bosn_workspace_test.py").read_text(encoding="utf-8")
    assert '".gate-nonce"' in task and "gate-nonce: " in task
    assert GATE.NONCE_FILE in (ROOT / ".gitignore").read_text(encoding="utf-8")


def test_the_test_suite_only_runs_isolated_locally() -> None:
    lanes = {check.name: check for check in GATE.checks()}
    tests = lanes["soldr tests (isolated, bosn)"]
    assert tests.argv == ("bosn", "run", "--task", "test")
    for check in GATE.checks():
        if check.lane != "tests":
            assert "nextest" not in check.argv and "ci-test" not in check.argv, check


@dataclass(frozen=True)
class WrapperRun:
    returncode: int
    stderr: str
    stdout: str


def _shim(args: list[str], native: str | None, **extra: str) -> WrapperRun:
    """Run nextest_wrapper.sh with stderr captured through a file, not a pipe
    (zackees/ci.yml PY-003). ``native`` sets SOLDR_NEXTEST_NATIVE_WRAPPER."""
    env = {
        k: v
        for k, v in os.environ.items()
        if k not in ("CI", "SOLDR_TEST_ISOLATED", "SOLDR_NEXTEST_NATIVE_WRAPPER")
    }
    env.update(extra)
    if native is not None:
        env["SOLDR_NEXTEST_NATIVE_WRAPPER"] = native
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        proc = subprocess.run(
            ["sh", str(WRAPPER), *args],
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


def _wrapper(**extra: str) -> WrapperRun:
    return _shim(["true"], "/bin/true", **extra)


def _echo_script(path: Path, label: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f'#!/bin/sh\necho "{label} $*"\n', encoding="utf-8")
    path.chmod(0o755)
    return path


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_isolation_guard_refuses_a_developer_host() -> None:
    refused = _wrapper()
    assert refused.returncode != 0
    assert "bosn run --task test" in refused.stderr
    assert _wrapper(CI="true").returncode == 0
    assert _wrapper(SOLDR_TEST_ISOLATED="1").returncode == 0


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_isolation_guard_runs_before_any_wrapper_resolution(tmp_path: Path) -> None:
    native = _echo_script(tmp_path / "native", "native")
    refused = _shim(["true"], str(native))
    assert refused.returncode == 97
    assert "native" not in refused.stdout


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_an_explicit_native_wrapper_is_execed_with_the_test_argv(
    tmp_path: Path,
) -> None:
    """soldr#3454 resolution rule, step 1: SOLDR_NEXTEST_NATIVE_WRAPPER wins."""
    native = _echo_script(tmp_path / "native", "native")
    profile = tmp_path / "target" / "debug"
    _echo_script(profile / "soldr-nextest-wrapper", "derived")
    test_binary = _echo_script(profile / "deps" / "suite-0123", "test")
    run = _shim([str(test_binary), "--exact", "a::b"], str(native), CI="true")
    assert run.returncode == 0, run.stderr
    assert run.stdout.strip() == f"native {test_binary} --exact a::b"


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
@pytest.mark.parametrize("runner", [[], ["/usr/bin/env"]])
def test_the_wrapper_is_found_beside_the_test_binary_profile_dir(
    tmp_path: Path, runner: list[str]
) -> None:
    """Step 2: `<profile>/deps/<test>` -> `<profile>/soldr-nextest-wrapper`.

    The same relative layout holds for a workspace build, a `--target` build
    (`<target>/<triple>/<profile>`) and an extracted Nextest archive, and the
    test binary is found even behind a target runner.
    """
    profile = tmp_path / "extract" / "target" / "x86_64-apple-darwin" / "ci-nextest"
    _echo_script(profile / "soldr-nextest-wrapper", "derived")
    test_binary = _echo_script(profile / "deps" / "suite-0123", "test")
    argv = [*runner, str(test_binary), "--exact", "a::b"]
    run = _shim(argv, None, SOLDR_TEST_ISOLATED="1")
    assert run.returncode == 0, run.stderr
    assert run.stdout.strip() == "derived " + " ".join(argv)


@pytest.mark.skipif(os.name == "nt", reason="the nextest run-wrapper is Unix-only")
def test_a_missing_wrapper_refuses_instead_of_running_the_test_unwrapped(
    tmp_path: Path,
) -> None:
    """Step 3: no override, nothing beside the test binary -> loud refusal."""
    test_binary = _echo_script(tmp_path / "target" / "debug" / "deps" / "t", "test")
    run = _shim([str(test_binary)], None, CI="true")
    assert run.returncode == 98
    assert "soldr-nextest-wrapper not found" in run.stderr
    assert "soldr cargo build -p soldr-nextest-wrapper" in run.stderr
    assert "test" not in run.stdout, "the test must not run unwrapped"


def test_isolated_suite_requires_a_bosn_that_does_not_reap_bursts() -> None:
    # zackees/bosn#317: bosn < 0.1.5 reaped `ci-test` on an output burst.
    tests = {check.name: check for check in GATE.checks()}[
        "soldr tests (isolated, bosn)"
    ]
    assert tests.min_version is not None and tests.min_version >= (0, 1, 6)


def test_main_publisher_promotes_merged_pr_evidence() -> None:
    """Merged PR trailers become main cache evidence without skipping main jobs."""
    pre = (ROOT / ".github" / "workflows" / "ci-pre.yml").read_text(encoding="utf-8")
    assert "--github-context" in pre
    assert "GITHUB_TOKEN: ${{ github.token }}" in pre
    assert "args=" not in pre.split("  attestations:", 1)[1]
    assert f"ref: {GATE.CI_LINT_REF}" in pre
