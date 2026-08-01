"""The documented escape hatch must name a real env var (soldr#2148).

`docs/DEBUG_SIDECARS.md` tells a Windows contributor to build with
`ZCCACHE_DISABLE=1` to get a `.pdb`, because a cached build drops it. That
instruction is only useful while the variable it names is the one soldr
actually honours — a rename would leave the doc confidently wrong, which is
exactly the failure mode the note exists to correct.

The section previously said "debug the resulting Soldr binary and its normal
`.pdb`", and on Windows that silently did not work at all. A doc that is wrong
about how to get symbols is worse than one that says nothing, because the
reader stops looking.

Deliberately *not* asserted here: that the bug still exists. Pinning the
workaround would make this test fail when soldr#2148 is fixed, turning a
successful fix into a red build. The note carries its own "remove when #2148
closes" instruction instead.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DOC = REPO_ROOT / "docs" / "DEBUG_SIDECARS.md"


def _documented_env_vars() -> set[str]:
    """SCREAMING_SNAKE names in the doc that look like env vars."""
    text = DOC.read_text(encoding="utf-8")
    return {
        name
        for name in re.findall(r"\b([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)\b", text)
        # Filter out prose acronyms and file formats that share the shape.
        if name.startswith(("SOLDR_", "ZCCACHE_", "RUSTC_", "CARGO_"))
    }


def _sources() -> str:
    parts = []
    for root in ("crates", ".github", "docs"):
        base = REPO_ROOT / root
        for path in base.rglob("*"):
            if path.suffix in {".rs", ".py", ".yml", ".yaml", ".md"} and path.is_file():
                parts.append(path.read_text(encoding="utf-8", errors="replace"))
    return "\n".join(parts)


def test_documented_env_vars_exist_in_the_tree() -> None:
    documented = _documented_env_vars()
    assert documented, "expected the doc to name at least one env var"

    haystack = _sources()
    unknown = sorted(name for name in documented if haystack.count(name) < 2)
    assert not unknown, (
        "docs/DEBUG_SIDECARS.md names env vars that appear nowhere else in the "
        f"tree, so the instructions cannot work: {unknown}"
    )


def test_the_windows_pdb_note_is_actionable() -> None:
    text = DOC.read_text(encoding="utf-8")
    # The note has to carry the command, or it states a problem without a way
    # out -- which is what the section did before soldr#2148.
    assert "ZCCACHE_DISABLE=1" in text, (
        "the Windows .pdb note must give the actual command that produces a "
        "symbolizable build"
    )
    assert "2148" in text, "the note must reference the issue it is tracking"
    # And it must say when to delete it, so it does not outlive the bug.
    assert re.search(r"[Rr]emove this note when .*2148", text), (
        "the note must say when to remove it; an obsolete workaround that "
        "nobody knows is obsolete is its own trap"
    )
