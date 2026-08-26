"""windows-gnu must not ship DWARF inside its CI images (soldr#2883).

`x86_64-pc-windows-gnu` is the only target soldr builds where rustc has no
sidecar to put debug info in:

    $ rustc --print split-debuginfo --target x86_64-pc-windows-gnu
    off
    $ rustc --print split-debuginfo --target x86_64-pc-windows-msvc
    packed

So `debug = "line-tables-only"` on the `ci-nextest` profile lands DWARF inside
every PE for that target, while MSVC writes a `.pdb` the nextest archive never
carries. Measured on one CI run of the same commit, the gnu test archive was
3,491,930,139 B against msvc's 248,561,683 B — 14x — and every process start
on the lane cost ~0.42 s against msvc's ~0.02 s.

Windows pays that twice: the broker copies the daemon image per route, and the
OS loads it per process. That is what put `soldr daemon start` past its 60 s
route budget on the gnu lane while the msvc lane ran the same test in 20.6 s.

The override that fixes it is one line of workflow env, which is exactly the
kind of thing that gets dropped in a future edit with no test to notice. Hence
this file.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
CROSS_BUILD = REPO_ROOT / ".github" / "workflows" / "_ci-cross-build-linux.yml"
TARGET_RUN = REPO_ROOT / ".github" / "workflows" / "_ci-target-run.yml"
CI = REPO_ROOT / ".github" / "workflows" / "ci.yml"

# Cargo's profile-override spelling: CARGO_PROFILE_<PROFILE>_<KEY>, with the
# profile name uppercased and `-` folded to `_`.
OVERRIDE = "CARGO_PROFILE_CI_NEXTEST_DEBUG"


@pytest.fixture(scope="module")
def cross_build_steps() -> list[dict]:
    doc = yaml.safe_load(CROSS_BUILD.read_text(encoding="utf-8"))
    jobs = doc["jobs"]
    assert len(jobs) == 1, f"expected one job, got {sorted(jobs)}"
    return next(iter(jobs.values()))["steps"]


def find_step(steps: list[dict], needle: str) -> dict:
    for step in steps:
        if needle in (step.get("run") or ""):
            return step
    raise AssertionError(f"no step runs {needle!r}")


def test_the_gnu_lane_drops_debuginfo(cross_build_steps: list[dict]) -> None:
    step = find_step(cross_build_steps, OVERRIDE)
    assert f"{OVERRIDE}=false" in step["run"], step["run"]
    assert "GITHUB_ENV" in step["run"], "the override must reach later steps"


def test_the_override_is_scoped_to_windows_gnu(cross_build_steps: list[dict]) -> None:
    """Scope is the whole point.

    Applied unconditionally this would strip soldr#1168's line tables from
    Linux and macOS target-run backtraces too — where rustc *does* offer a
    sidecar and the size problem does not exist.
    """
    condition = find_step(cross_build_steps, OVERRIDE).get("if", "")
    assert "pc-windows-gnu" in condition, condition
    assert "inputs.target" in condition, condition


def cross_built_targets() -> list[str]:
    """Every target `ci.yml` actually hands to the cross-build workflow.

    Read rather than hard-coded: a new triple must be evaluated against the
    condition automatically, or this test only ever checks the targets that
    existed when it was written.
    """
    doc = yaml.safe_load(CI.read_text(encoding="utf-8"))
    targets = [
        job["with"]["target"]
        for job in doc["jobs"].values()
        if isinstance(job, dict)
        and str(job.get("uses", "")).endswith("_ci-cross-build-linux.yml")
        and isinstance(job.get("with"), dict)
        and "target" in job["with"]
    ]
    assert targets, "found no cross-build jobs in ci.yml"
    return targets


def test_the_condition_selects_gnu_windows_targets_and_no_others() -> None:
    """Evaluate the real predicate against the real target list.

    `contains(inputs.target, 'pc-windows-gnu')` is a substring test, and the
    obvious wrong shortening — `'windows'` — silently also matches all three
    msvc triples. So this applies the actual needle to the actual targets
    instead of asserting on the condition's text.
    """
    steps = yaml.safe_load(CROSS_BUILD.read_text(encoding="utf-8"))
    steps = next(iter(steps["jobs"].values()))["steps"]
    condition = find_step(steps, OVERRIDE).get("if", "")
    match = re.search(r"contains\(\s*inputs\.target\s*,\s*'([^']+)'\s*\)", condition)
    assert match, f"condition is not a recognisable contains(): {condition!r}"
    needle = match.group(1)

    selected = [t for t in cross_built_targets() if needle in t]
    expected = [t for t in cross_built_targets() if t.endswith("-pc-windows-gnu")]
    assert (
        selected == expected
    ), f"needle {needle!r} selects {selected}, expected exactly {expected}"
    assert selected, "no windows-gnu target is being cross-built at all"


def test_the_reason_travels_with_the_override(cross_build_steps: list[dict]) -> None:
    """A bare env assignment would read as a tuning knob someone may retune.

    The `--print split-debuginfo` output is the fact that makes this override
    correct rather than arbitrary, so it has to be next to it.
    """
    step = find_step(cross_build_steps, OVERRIDE)
    body = (step.get("name") or "") + CROSS_BUILD.read_text(encoding="utf-8")
    assert "split-debuginfo" in body
    assert "soldr#2883" in body


def test_the_target_run_listing_reports_sizes() -> None:
    """The number that explains the lane must be visible in the lane's log."""
    doc = yaml.safe_load(TARGET_RUN.read_text(encoding="utf-8"))
    steps = next(iter(doc["jobs"].values()))["steps"]
    listing = next(
        step for step in steps if (step.get("name") or "") == "List artifact contents"
    )
    run = listing["run"]
    assert "-printf" in run, "the listing must print sizes, not just paths"
    # The fallback matters: `find -printf` is GNU-only and this step also runs
    # on the macOS runners, whose BSD find would otherwise fail the step.
    assert run.count("find artifact/") >= 2, run
