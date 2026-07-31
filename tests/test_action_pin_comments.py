"""One pinned action SHA must carry one version comment.

Every `uses: owner/repo@<40-hex>` in this repo is followed by a `# vN` comment.
That comment is the only human-readable statement of what is pinned -- nobody
recognises `043fb46d1a93...` on sight -- and Dependabot reads it to decide what
an update would be. When it disagrees with the SHA it is worse than absent,
because it reads as verified.

It did disagree. `actions/upload-artifact@043fb46d1a93` is v7 (the `v7` and
`v7.0.1` tags both point at it; `v4` is a different commit entirely), and 23
call sites said `# v7` while `benchmark-stats.yml` said `# v4`. A reviewer
auditing artifact-action majors there would have concluded that workflow was a
major version behind, and "fixing" it means changing a SHA that was already
correct.

The check is deliberately *internal consistency*, not "does this comment match
the upstream tag". The latter cannot work: pinning a specific patch
(`actions/cache@0400d5f644dc # v4.2.4`) is correct and good practice, yet the
floating `v4` tag has since moved past it, so a naive comparison flags every
well-behaved pin in the repo. Asking instead that the same bytes are described
the same way everywhere is exact, needs no network, and catches the real
defect.
"""

from __future__ import annotations

import collections
import re
from pathlib import Path

WORKFLOWS = sorted(
    (Path(__file__).resolve().parents[1] / ".github" / "workflows").glob("*.y*ml")
)

PIN = re.compile(
    r"uses:\s*(?P<repo>[A-Za-z0-9._-]+/[A-Za-z0-9._/-]+)"
    r"@(?P<sha>[0-9a-f]{40})\s*#\s*(?P<version>\S+)"
)


def _pins() -> "dict[tuple[str, str], dict[str, list[str]]]":
    """{(repo, sha): {version_comment: [workflow, ...]}}"""
    found: dict[tuple[str, str], dict[str, list[str]]] = collections.defaultdict(
        lambda: collections.defaultdict(list)
    )
    for workflow in WORKFLOWS:
        for match in PIN.finditer(workflow.read_text(encoding="utf-8")):
            # Sub-action paths (owner/repo/sub) ship from the same commit as
            # their parent, so key on the repository root.
            repo = "/".join(match.group("repo").split("/")[:2])
            found[(repo, match.group("sha"))][match.group("version")].append(
                workflow.name
            )
    return found


def test_a_pinned_sha_has_one_version_comment() -> None:
    conflicts = []
    for (repo, sha), versions in sorted(_pins().items()):
        if len(versions) > 1:
            detail = "; ".join(
                f"{v} in {', '.join(sorted(files))}"
                for v, files in sorted(versions.items())
            )
            conflicts.append(f"{repo}@{sha[:12]} is described as {detail}")
    assert (
        not conflicts
    ), "the same pinned commit is labelled inconsistently:\n  " + "\n  ".join(conflicts)


def test_the_scan_finds_the_pins() -> None:
    # Without this, a regex that stopped matching would make the assertion
    # above vacuously true -- the failure mode the check exists to catch.
    pins = _pins()
    assert len(pins) >= 15, f"expected many SHA-pinned actions, found {len(pins)}"
    assert any(repo == "actions/checkout" for repo, _ in pins)


# Deliberately NOT asserted here: that every SHA-pinned action carries a
# version comment at all. The 14 `zackees/setup-soldr` pins do not, by
# existing convention -- they have their own dedicated check in
# `tests/test_setup_soldr_pins.py`, which resolves them against the live `v0`
# tag. Adding that assertion would fail on main today, which makes it a new
# policy rather than a regression guard, and that is not this test's job.
