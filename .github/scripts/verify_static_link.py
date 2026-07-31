#!/usr/bin/env python3
"""Verify a musl release binary is actually statically linked (soldr#1060).

soldr#1060's whole premise is that the musl artifacts are the
"download and run anywhere" build: they sidestep
`/lib64/libc.so.6: version GLIBC_2.xx not found` precisely because they carry
no dynamic dependencies. Nothing checked that. A musl target that silently
picked up a dynamic link still *builds*, still passes tests on the modern CI
image, and only fails on the old distro the artifact exists to serve — which
is the worst place to find out.

This is the RFC's fourth acceptance item ("static-link verification step
(`readelf -d` must show 'no dynamic section') added to release CI"), which
stands alone: it guards the musl artifacts release-auto.yml already produces,
independent of the artifact-renaming and installer-default work the RFC
defers.

Usage:
    python3 .github/scripts/verify_static_link.py <binary> [<binary> ...]

Exits non-zero if any binary has a dynamic section.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys

# `readelf -d` prints exactly this for a fully static ELF. Matched
# case-insensitively on a normalized string so binutils wording drift in
# spacing or capitalisation does not silently turn the check into a no-op.
_NO_DYNAMIC_SECTION = "there is no dynamic section in this file"


def is_statically_linked(readelf_output: str) -> bool:
    """True when `readelf -d` output shows no dynamic section.

    Pure, so the decision is unit-tested without needing binutils or a real
    ELF on the machine running the tests.
    """
    normalized = " ".join(readelf_output.lower().split())
    return _NO_DYNAMIC_SECTION in normalized


def dynamic_dependencies(readelf_output: str) -> "list[str]":
    """The `NEEDED` shared libraries named in the output, for the failure
    message. A reader needs to know *what* leaked in, not just that
    something did."""
    found: list[str] = []
    for line in readelf_output.splitlines():
        if "(NEEDED)" not in line:
            continue
        # e.g. " 0x0001 (NEEDED)  Shared library: [libc.so.6]"
        start, end = line.find("["), line.rfind("]")
        if 0 <= start < end:
            found.append(line[start + 1 : end])
        else:
            found.append(line.strip())
    return found


def _readelf(binary: str) -> "tuple[int, str]":
    """Run `readelf -d`, returning `(exit_code, combined_output)`.

    The exit code matters on its own: readelf exits non-zero when it cannot
    read the file at all (missing path, not an ELF). Treating that as ordinary
    output would let a *non-existent* binary fall through to the
    "has a dynamic section" branch and report the wrong reason -- which is
    exactly what happened on Linux while a Windows box, having no readelf at
    all, took the missing-tool path and looked fine.
    """
    tool = shutil.which("readelf") or shutil.which("llvm-readelf")
    if tool is None:
        raise FileNotFoundError("readelf not found on PATH")
    completed = subprocess.run(
        [tool, "-d", binary],
        capture_output=True,
        text=True,
        check=False,
    )
    # readelf reports "no dynamic section" on stdout but still exits 0; a
    # genuinely unreadable file lands on stderr, so keep both.
    return completed.returncode, completed.stdout + completed.stderr


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="ELF binaries to verify")
    args = parser.parse_args(argv)

    failures = 0
    for binary in args.binaries:
        try:
            code, output = _readelf(binary)
        except (OSError, FileNotFoundError) as error:
            # A missing tool or unreadable path is a wiring problem. Fail
            # loudly rather than pass by default -- a verification step that
            # silently skips is worse than no step at all.
            print(
                f"verify_static_link: cannot inspect {binary}: {error}", file=sys.stderr
            )
            failures += 1
            continue

        if code != 0:
            # readelf could not read the file (missing path, not an ELF). We
            # learned nothing, so this is "cannot verify", not "dynamic".
            print(
                f"verify_static_link: cannot inspect {binary}: "
                f"readelf exited {code}: {output.strip().splitlines()[-1] if output.strip() else 'no output'}",
                file=sys.stderr,
            )
            failures += 1
            continue

        if is_statically_linked(output):
            print(f"verify_static_link: {binary} is statically linked - OK")
            continue

        failures += 1
        needed = dynamic_dependencies(output)
        print(
            f"verify_static_link: {binary} HAS a dynamic section, so it is not "
            f"the portable static artifact it is published as (soldr#1060).",
            file=sys.stderr,
        )
        for lib in needed:
            print(f"  needs: {lib}", file=sys.stderr)
        print(
            "  A musl target must link statically; a dynamic dependency here "
            "reproduces the GLIBC-version failure the musl build exists to avoid.",
            file=sys.stderr,
        )

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
