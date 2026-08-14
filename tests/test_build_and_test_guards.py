"""Regression tests for the two assertion guards in `_build-and-test.yml`.

Both steps assert something that never fails loudly on its own:

* soldr#1838 -- the build silently ran uncached (a daemon fallback);
* soldr#1799 -- soldr's managed toolchain homes leaked onto a host-resolved
  tool, which flips which rustc runs, invalidates cargo fingerprints and
  zccache keys, and leaves warm builds recompiling the world.

They are deliberately gated differently, and the difference is easy to erase
by copying one line from the other:

* #1838 is advisory off `linux-gnu`, because a flaky Windows/macOS daemon
  race should surface in the log without gating the build. That is a stated
  platform story.
* #1799 is unconditional, because `home_origin` correctness is the CLAUDE.md
  invariant on every platform. It carried the neighbour's advisory
  expression until soldr#1799 follow-up work removed it.

Neither expression is live today -- `ci.yml` is the only caller and passes
`x86_64-unknown-linux-gnu` -- so nothing at runtime would notice them
drifting. Hence a test.

Plain-text parsing, matching `test_thin_v2_verify_workflow.py`: pyyaml is not
a dependency here, and a guard that skips when a module is missing is the
failure mode these very steps exist to prevent.
"""

from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml"

GUARD_1799 = "Assert managed toolchain homes did not leak (soldr#1799)"
GUARD_1838 = "Assert the build did not silently run uncached (soldr#1838)"


def _step_body(workflow: str, step_name: str) -> str:
    """The lines of one step, from its `- name:` to the next step's."""
    start = workflow.index(f"- name: {step_name}")
    nxt = workflow.find("\n      - name: ", start + 1)
    return workflow[start : nxt if nxt != -1 else len(workflow)]


def test_the_toolchain_home_guard_is_unconditional() -> None:
    # soldr#1799 acceptance: "CI workflow fails on any host-resolved tool
    # executing under managed homes" -- on any platform, not just Linux.
    body = _step_body(WORKFLOW.read_text(encoding="utf-8"), GUARD_1799)
    assert "continue-on-error" not in body, (
        "the #1799 toolchain-home guard must hard-fail everywhere; an "
        "advisory gate would let a home leak land silently, which is the "
        "entire failure mode the issue exists for"
    )


def test_the_uncached_build_guard_keeps_its_documented_platform_split() -> None:
    # The inverse: #1838 IS advisory off linux-gnu on purpose, and flipping it
    # to unconditional would gate builds on a known-flaky off-Linux race.
    body = _step_body(WORKFLOW.read_text(encoding="utf-8"), GUARD_1838)
    assert "continue-on-error" in body, (
        "the #1838 guard is intentionally advisory off linux-gnu; making it "
        "unconditional would gate the build on a flaky daemon race"
    )
    assert "linux-gnu" in body


def test_both_guards_still_exist() -> None:
    # Cheap protection against the step-body lookup silently matching nothing
    # if a step is renamed -- these assertions are only meaningful while the
    # steps they name are present.
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert workflow.count(f"- name: {GUARD_1799}") == 1
    assert workflow.count(f"- name: {GUARD_1838}") == 1


def test_the_guards_run_the_scripts_they_claim_to() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "check_toolchain_homes.py" in _step_body(workflow, GUARD_1799)
    assert "check_compile_fallbacks.py" in _step_body(workflow, GUARD_1838)


def test_hosted_runner_compile_concurrency_is_memory_bounded() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert 'CARGO_BUILD_JOBS: "1"' in workflow
    assert 'SOLDR_JOBS: "1"' in workflow
