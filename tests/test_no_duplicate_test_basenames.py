"""Two test files may not share a basename across pytest's collected roots.

soldr#2829: `tests/test_win_gnu_link_smoke.py` was added beside a
`.github/scripts/test_win_gnu_link_smoke.py` that had existed since #2346.
Neither directory holds an `__init__.py`, so pytest derives a module name from
the basename alone, the second file collides with the first, and collection
does not merely skip it -- it **aborts**:

    import file mismatch:
    imported module 'test_win_gnu_link_smoke' has this __file__ attribute: ...
    HINT: remove __pycache__ / .pyc files and/or use a unique basename

One aborted collection fails the whole `Python tests` step, which is the Lint
job, which gates every PR in the repo. The cost is wildly out of proportion to
the mistake, and the mistake is invisible in review: each file is fine on its
own and the diff that adds the second one never mentions the first.

The failure is also confusing enough to be misread. The message names
`__pycache__` first, so the natural reaction is to delete caches and conclude
it was a local artifact -- but the collision is deterministic and reproduces on
a fresh CI checkout.
"""

from __future__ import annotations

import collections
import pathlib

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]

# The roots the Lint job passes to pytest. Keep in step with ci.yml's
# `python -m pytest tests/ .github/scripts/` invocation.
COLLECTED_ROOTS = ("tests", ".github/scripts")


def collected_test_files() -> list[pathlib.Path]:
    """Every `test_*.py` pytest would import from the roots above."""
    found: list[pathlib.Path] = []
    for root in COLLECTED_ROOTS:
        directory = REPO_ROOT / root
        if not directory.is_dir():
            continue
        found.extend(sorted(directory.rglob("test_*.py")))
    return found


def test_the_scan_reaches_both_roots() -> None:
    """A guard that scans nothing reports clean, which is worse than no guard.

    soldr#2008 shipped exactly that. Asserting each root contributes files
    means a renamed directory or a changed pytest invocation fails here rather
    than silently disarming the check below.
    """
    for root in COLLECTED_ROOTS:
        directory = REPO_ROOT / root
        assert directory.is_dir(), f"collected root is missing: {root}"
        matches = list(directory.rglob("test_*.py"))
        assert matches, f"no test files found under {root}"


def test_no_two_test_files_share_a_basename() -> None:
    by_basename: dict[str, list[str]] = collections.defaultdict(list)
    for path in collected_test_files():
        by_basename[path.name].append(str(path.relative_to(REPO_ROOT)))

    collisions = {
        name: paths for name, paths in by_basename.items() if len(paths) > 1
    }

    assert not collisions, (
        "test files sharing a basename break pytest collection for the whole "
        "run, not just for themselves:\n"
        + "\n".join(
            f"  {name}\n" + "".join(f"    {path}\n" for path in sorted(paths))
            for name, paths in sorted(collisions.items())
        )
        + "Give one of them a distinct name, or add __init__.py to both "
        "directories to make the module paths unique."
    )
