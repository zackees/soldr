#!/usr/bin/env python3
"""Keep the per-PR glibc ceilings in agreement (soldr#2145).

Three workflow steps assert the glibc floor of a binary **soldr itself
builds**, and they must agree — they are three measurements of one fact:

  _ci-cross-build-linux.yml        aarch64 cross-build, debug profile
  cross-compile-all-targets.yml    x86_64 host-native, release profile
  cross-compile-all-targets.yml    aarch64 cross-build, release profile

They arrived in three separate PRs (#2145's lane, then soldr#2163, then
soldr#2165) and nothing tied them together, so one could be tightened or
loosened alone and the others would keep reporting the old number.

`release-auto.yml` is deliberately **excluded**, and the reason is the
whole point of this script rather than an exception to it. Its ceiling
also covers `crgx` and `cargo-chef`, which soldr does not build — it
fetches them prebuilt from the soldr-toolchain catalogue, and measuring
the published v0.8.30 bundle shows both at 2.39. So the release ceiling
is pinned by third-party build choices and legitimately differs from
what soldr's own binaries achieve. soldr#2170 tried to lower it and was
closed for exactly this.

`verify_wheel_glibc.py` is a different script against different
artifacts (manylinux wheels, 2.17) and is out of scope.

Usage:
  python3 .github/scripts/check_glibc_ceilings.py [--repo-root PATH]

Exit codes:
  0 — every own-binary ceiling agrees
  1 — they disagree, or an expected invocation vanished
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

SCRIPT = "verify_glibc_baseline.py"

# Workflows whose `verify_glibc_baseline.py` invocations gate a binary
# soldr builds. `release-auto.yml` is excluded on purpose — see module docs.
OWN_BINARY_WORKFLOWS = (
    ".github/workflows/_ci-cross-build-linux.yml",
    ".github/workflows/cross-compile-all-targets.yml",
)

# Losing these silently would be worse than a mismatch: the lanes would go
# green because nothing is checked any more.
MIN_EXPECTED_INVOCATIONS = 3

CEILING_RE = re.compile(r"--max-glibc\s+([0-9][0-9.]*)")


def ceilings_in(text: str) -> list[str]:
    """Every `--max-glibc` passed to SCRIPT in `text`.

    An invocation is usually split across continuation lines, so the flag
    is looked for in a small window after the script name rather than on
    the same line.
    """
    found: list[str] = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if SCRIPT not in line:
            continue
        window = "\n".join(lines[index : index + 4])
        match = CEILING_RE.search(window)
        if match:
            found.append(match.group(1))
    return found


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", default=".", type=Path)
    args = parser.parse_args(argv)

    seen: dict[str, list[str]] = {}
    for relative in OWN_BINARY_WORKFLOWS:
        path = args.repo_root / relative
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:
            print(
                f"check_glibc_ceilings: cannot read {relative}: {error}",
                file=sys.stderr,
            )
            return 1
        found = ceilings_in(text)
        if found:
            seen[relative] = found

    total = sum(len(v) for v in seen.values())
    if total < MIN_EXPECTED_INVOCATIONS:
        print(
            f"check_glibc_ceilings: expected at least {MIN_EXPECTED_INVOCATIONS} "
            f"{SCRIPT} invocations across {', '.join(OWN_BINARY_WORKFLOWS)}, found {total}. "
            "A ratchet was removed or renamed — if that was deliberate, update "
            "MIN_EXPECTED_INVOCATIONS with a note saying why.",
            file=sys.stderr,
        )
        for relative, found in seen.items():
            print(f"  {relative}: {found}", file=sys.stderr)
        return 1

    distinct = sorted({value for values in seen.values() for value in values})
    if len(distinct) != 1:
        print(
            f"check_glibc_ceilings: the per-PR ceilings disagree: {', '.join(distinct)}. "
            "These gate binaries soldr builds, so they are three measurements of one "
            "fact and must move together.",
            file=sys.stderr,
        )
        for relative, found in seen.items():
            print(f"  {relative}: {found}", file=sys.stderr)
        return 1

    print(
        f"check_glibc_ceilings: {total} own-binary ceilings agree at {distinct[0]} - OK"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
