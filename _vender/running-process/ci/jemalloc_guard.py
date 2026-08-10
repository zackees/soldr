"""Guard: jemalloc is gone, and may not come back.

Heap profiling runs on `mimalloc-pprof`, which emits pprof directly. The
jeprof text parser and its pprof lowering stage were deleted rather than
ported, because the allocator now produces the wire format itself.

This began as a ratchet over the sites that still existed (#792). They are all
gone, so `KNOWN_SITES` is empty and the check is now absolute: any jemalloc
reference in tracked source fails lint. Re-introducing it would mean
re-introducing a text format the daemon no longer speaks.

Run alone with:
    uv run --no-project python -m ci.jemalloc_guard
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Tokens that mean "jemalloc is involved here". `heap_v2` and `jeprof` are
# jemalloc's dump format; `MALLOC_CONF` / `_RJEM_` are how its profiler is
# switched on; `tikv-jemalloc` is the crate family.
PATTERN = re.compile(
    r"jemalloc|jeprof|MALLOC_CONF|_RJEM_|heap_v2|tikv-jemalloc", re.IGNORECASE
)

# Extensions worth scanning. Deliberately excludes Cargo.lock: it is generated,
# and its jemalloc entries disappear on their own once the manifests stop
# asking for the crates.
SUFFIXES = {".rs", ".toml", ".md", ".py", ".yml", ".yaml", ".json", ".sh"}

# This guard's own module name, which its callers must spell out, plus the
# `python -m ci` stage name that runs it (#516).
#
# Stripped before counting rather than exempting `ci/lint.py`,
# `tests/test_ci_lint.py`, or `ci/__main__.py` wholesale: those files should
# still be caught if they ever gain a *real* jemalloc reference. Only the name
# of the machinery that removes jemalloc is ignored, not every mention in the
# files that invoke it.
#
# `guard-jemalloc` is listed separately because it is not spelled
# `jemalloc_guard` — the stage name reads better hyphen-first, and the shorter
# pattern would not cover it.
SELF_REFERENCE = re.compile(r"jemalloc_guard|guard-jemalloc")

# The guard itself is exempt outright — it has to spell out every token it
# forbids in order to search for them.
EXEMPT = {"ci/jemalloc_guard.py"}

# The jemalloc that exists today, path -> occurrence count.
#
# Every entry is a thing the purge has to delete. Shrinking a number is fine
# and needs no edit here (the check is `>`), but removing the last reference in
# a file should also remove its row, so the remaining work stays readable.
KNOWN_SITES: dict[str, int] = {}


def _relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def _iter_source_files() -> list[Path]:
    """Source files git knows about: tracked, plus new files not yet ignored.

    Deliberately not a filesystem walk. This repo keeps a vendored toolchain
    and crate registry under `.cargo/` and `.rustup/` — both gitignored, both
    full of jemalloc's own source. Walking the tree found ~200 "violations" in
    upstream crates that no purge could ever remove.

    `--others --exclude-standard` matters as much as `--cached`. With tracked
    files alone, a brand-new file is invisible until it is committed — so the
    guard reports a clean tree, the file lands, and lint only fails *after* the
    merge. That is exactly how a jemalloc-discussing doc comment reached main
    in #794: the fixture was new, therefore untracked, therefore unscanned.
    Honouring .gitignore keeps the vendored toolchains out.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            "jemalloc-guard: `git ls-files` failed; this guard scans tracked "
            f"files and needs a git checkout.\n{result.stderr.decode(errors='replace')}"
        )

    found: list[Path] = []
    for entry in result.stdout.decode(errors="replace").split("\0"):
        if not entry:
            continue
        path = ROOT / entry
        if path.suffix in SUFFIXES and path.is_file():
            found.append(path)
    return found


def count_occurrences(path: Path) -> int:
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return 0
    return len(PATTERN.findall(SELF_REFERENCE.sub("", text)))


def scan() -> dict[str, int]:
    """Every scanned file that mentions jemalloc, with its occurrence count."""
    counts: dict[str, int] = {}
    for path in _iter_source_files():
        if _relative(path) in EXEMPT:
            continue
        found = count_occurrences(path)
        if found:
            counts[_relative(path)] = found
    return counts


def check() -> list[str]:
    failures: list[str] = []
    actual = scan()

    for rel, found in sorted(actual.items()):
        allowed = KNOWN_SITES.get(rel)
        if allowed is None:
            failures.append(
                f"{rel}: {found} jemalloc reference(s).\n"
                "    jemalloc was removed in favour of `mimalloc-pprof` (#792) "
                "and may not return:\n"
                "    the daemon no longer has a jeprof parser for it to feed."
            )
        elif found > allowed:
            failures.append(
                f"{rel}: {found} jemalloc reference(s), up from {allowed}.\n"
                "    This file is on the removal list; it should be shrinking, "
                "not growing."
            )

    # A stale row is its own bug: it makes the remaining work look larger than
    # it is, and it is how a ratchet quietly stops ratcheting.
    for rel in sorted(KNOWN_SITES):
        if rel not in actual:
            failures.append(
                f"{rel}: listed in KNOWN_SITES but now has no jemalloc "
                "references.\n"
                "    Delete its row — the list should shrink as the purge "
                "proceeds."
            )
    return failures


def main() -> int:
    failures = check()
    if failures:
        print("jemalloc-guard: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    remaining = sum(KNOWN_SITES.values())
    if remaining:
        print(
            f"jemalloc-guard: ok — {len(KNOWN_SITES)} file(s), "
            f"{remaining} reference(s) still to remove."
        )
    else:
        print("jemalloc-guard: ok — no jemalloc references remain.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
