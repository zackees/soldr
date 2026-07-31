"""A workflow that reaches macOS must not use bash-4-only builtins.

macOS ships **bash 3.2** and always will: bash went GPLv3 at 4.0 and Apple will
not ship that. So `mapfile` / `readarray`, introduced in bash 4.0, do not exist
on a native macOS runner.

This is not hypothetical -- it stopped the v0.8.30 release:

    /Users/runner/work/_temp/....sh: line 2: mapfile: command not found
    ##[error]Process completed with exit code 127

in `Ratchet the macOS minimum version (soldr#1060)`. Publishing is gated on
`build.result == 'success'`, so nothing shipped; the release just stopped.

Two things let it survive introduction unnoticed:

1. **The step only runs during a release.** Every PR parsed the same workflow
   file without ever reaching that job, so CI was green the whole time it was
   broken. It failed the first time it was ever executed.
2. **The sibling darwin lane passed.** `macOS x64 (cross-compiled)` builds on a
   *Linux* runner, where bash is 5.x, so an `apple-darwin`-gated step ran there
   happily. Only the native `macOS ARM64` lane has bash 3.2 -- which is why
   "the other darwin lane is fine" was not evidence of anything.

The rule is per **file**, not per step, deliberately. Deciding which steps can
land on macOS means resolving `runs-on`, matrix includes and `if:` expressions,
and a rule that subtle is one refactor away from silently not applying. "This
file runs on macOS somewhere, so its shell stays bash-3.2-portable" needs no
resolution and cannot drift. The portable loop costs three lines and works on
every runner, so there is nothing to trade away.

Stdlib only, like the other workflow guards here -- pyyaml is not installed in
CI's lint environment, and a guard that silently skips is worse than no guard.
"""

from __future__ import annotations

import re
from pathlib import Path

WORKFLOWS = sorted(
    (Path(__file__).resolve().parents[1] / ".github" / "workflows").glob("*.y*ml")
)

# Introduced in bash 4.0. Replacement:
#   arr=()
#   while IFS= read -r line; do arr+=("$line"); done < <(...)
BASH4_ONLY = ("mapfile", "readarray")

# A runner label naming macOS: `runs-on: macos-15`, `macos-latest`, a matrix
# `runner: macos-14`, etc.
MACOS_RUNNER = re.compile(r"\bmacos-[a-z0-9.]+\b", re.IGNORECASE)


def _reaches_macos(text: str) -> bool:
    return bool(MACOS_RUNNER.search(text))


def _bash4_command_lines(text: str) -> "list[tuple[int, str]]":
    """Lines where a bash-4-only builtin is in *command* position.

    Comments are skipped: the word also appears in the comment explaining why
    it is not used, and flagging that would make the fix look like the bug.
    """
    hits = []
    for number, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        for builtin in BASH4_ONLY:
            if stripped.startswith(builtin + " "):
                hits.append((number, stripped))
    return hits


def test_macos_capable_workflows_avoid_bash4_builtins() -> None:
    offenders = []
    for workflow in WORKFLOWS:
        text = workflow.read_text(encoding="utf-8")
        if not _reaches_macos(text):
            continue
        for number, line in _bash4_command_lines(text):
            offenders.append(f"{workflow.name}:{number}: {line}")
    assert not offenders, (
        "macOS ships bash 3.2, which has no `mapfile`/`readarray`; these would "
        "fail with 'command not found' (exit 127) on a native macOS runner:\n  "
        + "\n  ".join(offenders)
        + '\nUse: arr=(); while IFS= read -r line; do arr+=("$line"); done < <(...)'
    )


def test_the_scan_still_sees_macos_workflows_and_command_lines() -> None:
    # Without this, a regex that stopped matching would make the assertion above
    # vacuously true -- the failure mode the guard exists to catch.
    macos_workflows = [
        w.name for w in WORKFLOWS if _reaches_macos(w.read_text(encoding="utf-8"))
    ]
    # Four today. The bar is deliberately below that: this asserts the scan is
    # still finding workflows at all, not the exact inventory, which would turn
    # every unrelated workflow addition into a failure here.
    assert (
        len(macos_workflows) >= 3
    ), f"expected several macOS workflows, got {macos_workflows}"
    assert "release-auto.yml" in macos_workflows, (
        "release-auto.yml is the workflow this guard exists for -- if it is no "
        f"longer detected as macOS-capable the rule is not applying: {macos_workflows}"
    )

    # And the command-position detector must actually fire on the real shape.
    sample = "foo=()\nmapfile -t foo < <(find .)\n# mapfile is not used here\n"
    hits = _bash4_command_lines(sample)
    assert len(hits) == 1, f"detector should find exactly the command line, got {hits}"
    assert hits[0][0] == 2
