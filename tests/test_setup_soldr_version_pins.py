"""Every lane that builds with soldr must pin which soldr (soldr#1799).

Historically, setup-soldr defaulted to 0.8.23, which predates
`3dc25a94 fix(gc): retire automatic target pruning (#1820)` — first released in
0.8.24. Under 0.8.23 soldr garbage-collects live artifacts out of `target/`
after every build, so the *next* build recompiles the whole workspace.

This was measured, not assumed. From the CI log of a run whose two build steps
issue byte-identical commands:

    build 1   0 `Compiling` lines                      <- genuinely warm
              target-gc (after): pruned 91 families, 43.0 MB
    build 2   163 crate(s) recompiled
              target-gc (after): pruned 89 families, 41.7 MB

The tell is the second prune reclaiming the same amount again. Genuinely stale
families would collapse to ~0 on the second pass; instead build 2 recreated
what gc had just deleted and gc deleted it again. Steady-state loop.

Nothing fails when this happens. The lane is green and merely 10-50x slower,
forever, which is precisely the #1799 symptom and precisely why it survived
this long. There is corroborating evidence in the tree that it was felt but not
traced: the dylint steps in `ci.yml` carry `SOLDR_NO_GC_TARGET: "1"` with a
comment blaming "v0.8.23, whose post-command target pruning can remove Dylint's
cdylib" — a local workaround for the global bug.

So: pin the version at every call site that builds, and keep them agreeing.
"""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOWS = sorted(
    (Path(__file__).resolve().parents[1] / ".github" / "workflows").glob("*.y*ml")
)

# A `uses:` KEY. A comment merely mentioning the action is not a call site --
# getting this wrong is how the first version of this audit invented call sites
# at line numbers that held comments.
USES = re.compile(r"^(\s*)uses:\s*zackees/setup-soldr@")
VERSION = re.compile(r"\s*version:\s*(\S+)")

# Call sites that must NOT be pinned, and why. Each entry is load-bearing: an
# unexplained absence from this list is what the test is looking for.
EXEMPT = {
    # This workflow exists to test the action itself, so it has to exercise the
    # action's own default. Pinning it would make it stop testing the thing it
    # is named for.
    "setup-soldr-action.yml": "tests the action's default resolution",
    # A measurement baseline, pinned to 0.7.28 on purpose so the cache-delta
    # numbers stay comparable across runs. It is pinned, just not to our
    # version, so it is exempt from the agreement check rather than from
    # pinning.
    "cache-delta-experiment.yml": "0.7.28 experiment baseline",
}


def _call_sites() -> "list[tuple[str, int, str | None]]":
    """[(workflow, line, pinned_version_or_None)] for real `uses:` keys."""
    sites = []
    for workflow in WORKFLOWS:
        lines = workflow.read_text(encoding="utf-8").splitlines()
        for index, line in enumerate(lines):
            match = USES.match(line)
            if not match:
                continue
            indent = len(match.group(1))
            version = None
            for candidate in lines[index + 1 :]:
                if not candidate.strip() or candidate.lstrip().startswith("#"):
                    continue
                current = len(candidate) - len(candidate.lstrip())
                if current <= indent and candidate.lstrip().startswith("-"):
                    break  # next step; this one had no `version:`
                found = VERSION.match(candidate)
                if found:
                    version = found.group(1).strip("\"'")
                    break
            sites.append((workflow.name, index + 1, version))
    return sites


def test_every_building_call_site_pins_a_version() -> None:
    unpinned = [
        f"{name}:{line}"
        for name, line, version in _call_sites()
        if version is None and name not in EXEMPT
    ]
    assert not unpinned, (
        "these setup-soldr call sites take the action's default version, which "
        "GCs live target/ artifacts and makes every build recompile the world "
        "(soldr#1799): " + ", ".join(unpinned) + ". Pin `version:`, or add the "
        "workflow to EXEMPT in this file with the reason."
    )


def test_pinned_versions_agree() -> None:
    versions: dict[str, list[str]] = {}
    for name, line, version in _call_sites():
        if version is None or name in EXEMPT:
            continue
        versions.setdefault(version, []).append(f"{name}:{line}")
    assert len(versions) <= 1, (
        "lanes are building with different soldr versions, so a cache or "
        "behaviour difference between them is unattributable: "
        + "; ".join(f"{v} at {', '.join(w)}" for v, w in sorted(versions.items()))
    )


def test_the_pin_is_new_enough_for_catalogue_v2() -> None:
    for name, line, version in _call_sites():
        if version is None or name in EXEMPT:
            continue
        parts = tuple(int(p) for p in version.split("."))
        assert parts >= (0, 9, 5), (
            f"{name}:{line} pins soldr {version}, which predates catalogue v2. "
            "The live non-LFS publication intentionally has no catalogue.v1.json."
        )


def test_non_action_bootstraps_are_catalogue_v2_capable() -> None:
    root = Path(__file__).resolve().parents[1]
    expected = "0.9.6"
    assert f'"soldr=={expected}"' in (root / "pyproject.toml").read_text(
        encoding="utf-8"
    )
    assert f'SOLDR_VERSION = "{expected}"' in (
        root / "ci/win_wheel_local.py"
    ).read_text(encoding="utf-8")
    assert f"ENV SOLDR_VERSION={expected}" in (
        root / "ci/docker-aarch64-windows-msvc-cross/Dockerfile"
    ).read_text(encoding="utf-8")
    assert f"soldr=={expected}" in (
        root / ".github/workflows/docker-linux-cross-smoke.yml"
    ).read_text(encoding="utf-8")
    build_all = (root / ".github/workflows/build-all-from-linux.yml").read_text(
        encoding="utf-8"
    )
    assert f'default: "{expected}"' in build_all
    assert f"inputs.soldr_version || '{expected}'" in build_all


def test_the_scan_finds_the_call_sites() -> None:
    # Without this, a regex that stopped matching would make every assertion
    # above vacuously true -- the failure mode these checks exist to catch.
    sites = _call_sites()
    assert len(sites) >= 8, f"expected many setup-soldr call sites, found {len(sites)}"
    assert any(name == "ci.yml" for name, _, _ in sites)
    # and the exemptions must still refer to workflows that exist
    present = {workflow.name for workflow in WORKFLOWS}
    stale = sorted(set(EXEMPT) - present)
    assert not stale, f"EXEMPT names workflows that no longer exist: {stale}"
