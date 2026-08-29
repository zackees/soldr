#!/usr/bin/env python3
"""Every workflow `paths:` filter must match something in the tree.

A trigger filter that matches nothing is the quietest CI failure there is: the
workflow does not run, nothing goes red, and the gate is simply gone. On a PR
summary it is indistinguishable from a gate that ran and passed.

Not hypothetical. `crates/soldr-cli/src/cache_lib/**` moved to
`crates/soldr-cache/` in the #1490 Phase 4 workspace split (commit 9aefa762),
and two workflows kept watching the old location:

`cook-size-gate.yml` named `cache_lib/strip_target.rs` directly, and a
since-removed target-cache slice verifier watched the same library. Both had
been dark for changes to the code they exist to guard ever since, and neither
ever went red, because a workflow that never triggers cannot.

The check is about *existence*, not correctness: it cannot tell whether a
filter watches the right thing, only that it watches something real. That is
enough to catch a rename, which is how this rot happens.

Stdlib only, and hand-parsed rather than via PyYAML, for the same reason as
`verify_ci_job_timeouts.py`: this has to run in the Lint job without adding a
dependency. A pyyaml-based version of this check silently *skipped* when the
module was absent, which is the exact failure mode it exists to prevent.

Usage:
    python3 .github/scripts/verify_workflow_paths.py [--workflows DIR]

Exit codes:
  0 - every filter matches at least one path
  1 - a filter matches nothing, or no filters were found at all
"""

from __future__ import annotations

import argparse
import glob
import re
import sys
from pathlib import Path

PATHS_KEY = re.compile(r"^(\s*)(paths|paths-ignore):\s*(?:#.*)?$")
LIST_ITEM = re.compile(r"^(\s*)-\s+(.*?)\s*$")


def _strip_scalar(raw: str) -> str:
    """Unquote a YAML scalar and drop any trailing comment."""
    value = raw.strip()
    if value[:1] in {'"', "'"}:
        quote = value[0]
        end = value.find(quote, 1)
        if end > 0:
            return value[1:end]
        return value[1:]
    # Unquoted: a ` #` begins a comment.
    comment = value.find(" #")
    if comment >= 0:
        value = value[:comment]
    return value.strip()


def path_filters(workflow_text: str) -> "list[str]":
    """Every `paths:` / `paths-ignore:` pattern in a workflow.

    Indentation-based, matching the shape GitHub requires: a `paths:` key
    followed by list items indented further than the key.
    """
    patterns: list[str] = []
    lines = workflow_text.splitlines()
    index = 0
    while index < len(lines):
        header = PATHS_KEY.match(lines[index])
        if not header:
            index += 1
            continue
        key_indent = len(header.group(1))
        index += 1
        while index < len(lines):
            line = lines[index]
            if not line.strip() or line.lstrip().startswith("#"):
                index += 1
                continue
            item = LIST_ITEM.match(line)
            if not item or len(item.group(1)) <= key_indent:
                break
            value = _strip_scalar(item.group(2))
            if value:
                patterns.append(value)
            index += 1
    return patterns


def matches_something(pattern: str, root: Path) -> bool:
    if pattern.startswith("!"):
        # Negation; the positive form it refines is what gets checked.
        return True
    if (root / pattern).exists():
        return True
    return bool(glob.glob(str(root / pattern), recursive=True))


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workflows",
        type=Path,
        default=None,
        help="workflow directory (default: .github/workflows beside this script)",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="repository root the patterns are relative to",
    )
    args = parser.parse_args(argv)

    here = Path(__file__).resolve().parents[2]
    workflows = args.workflows or (here / ".github" / "workflows")
    root = args.root or (workflows.parent.parent)

    files = sorted(list(workflows.glob("*.yml")) + list(workflows.glob("*.yaml")))
    if not files:
        print(f"verify_workflow_paths: no workflows under {workflows}", file=sys.stderr)
        return 1

    total = 0
    failures = 0
    for workflow in files:
        patterns = path_filters(workflow.read_text(encoding="utf-8"))
        total += len(patterns)
        stale = [p for p in patterns if not matches_something(p, root)]
        if stale:
            failures += 1
            print(
                f"verify_workflow_paths: {workflow.name} filters on "
                f"{len(stale)} path(s) that match nothing, so it will not "
                f"trigger for them:",
                file=sys.stderr,
            )
            for pattern in stale:
                print(f"  {pattern}", file=sys.stderr)
            print(
                "  A renamed or moved path leaves the workflow silently "
                "dark -- it cannot go red, because it never runs.",
                file=sys.stderr,
            )

    if total == 0:
        # Finding nothing means the parser broke, not that the repo is clean.
        print(
            "verify_workflow_paths: parsed no path filters at all, which "
            "means this check stopped checking",
            file=sys.stderr,
        )
        return 1

    if failures:
        return 1

    print(
        f"verify_workflow_paths: {total} path filter(s) across "
        f"{len(files)} workflow(s) all match - OK"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
