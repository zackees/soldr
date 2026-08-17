"""Debug-sidecar docs must stay truthful about the cache (soldr#2148, #2424).

`docs/DEBUG_SIDECARS.md` once carried a `ZCCACHE_DISABLE=1` workaround for
cached Windows builds dropping their `.pdb` (soldr#2148). That bug is fixed —
the vendored cache declares `<image>.pdb` as a link output and replays it —
and soldr#2424 purged the stale cache-bypass advice. The guards here point in
both directions now:

- every env var the doc still names must exist in the tree, so instructions
  cannot rot into confidently-wrong (a rename leaves the doc lying); and
- the retired bypass advice must not quietly reappear. The sanctioned story
  is that cached builds retain their sidecars; recommending a cache bypass
  again should be a deliberate, reviewed decision, not a stale-doc revert.
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


def test_the_retired_pdb_cache_bypass_does_not_return() -> None:
    text = DOC.read_text(encoding="utf-8")
    # soldr#2148 is fixed: cached Windows builds retain the .pdb. The old
    # ZCCACHE_DISABLE workaround must stay gone (soldr#2424) — a bypass
    # recommendation reappearing here would either be stale-doc drift or a
    # regression report that belongs in an issue, not a workaround note.
    assert "ZCCACHE_DISABLE" not in text, (
        "docs/DEBUG_SIDECARS.md recommends a cache bypass again; the cached "
        "path retains debug sidecars (soldr#2148), so either the doc "
        "regressed or a real new bug needs an issue instead of a workaround"
    )
    # The positive claim stays documented so a Windows contributor knows the
    # cached path is supported for symbolized debugging.
    assert re.search(
        r"[Cc]ached builds retain", text
    ), "the doc must state that cached builds retain their debug sidecars"
