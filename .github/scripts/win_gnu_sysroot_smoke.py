#!/usr/bin/env python3
"""Smoke-verify the host-neutral MinGW-w64 sysroot soldr materializes on a
non-Windows host (soldr#2336 / soldr-toolchain#114).

The scheduled `win-gnu-sysroot.yml` lane runs `soldr prepare --target
x86_64-pc-windows-gnu` on a Linux runner, which (with the soldr#2336 branch)
downloads the host-neutral `mingw-w64-sysroot` bundle instead of hard-erroring.
This script asserts that the materialized tree actually contains the bits a
win-gnu link needs — a header, the import libraries, a CRT startup object, and
the gcc runtime — so a truncated or drifted asset is caught here rather than
silently much later inside a consumer's link.

Kept out of the workflow YAML per CLAUDE.md ("complex CI logic ... must be moved
to a `.github/scripts/*.py`"); unit-tested by `test_win_gnu_sysroot_smoke.py`.

Subcommands
-----------
  locate --soldr-home <dir>       print the materialized `package/` dir (globs
                                  the version segment); errors on 0 or >1 match.
  verify --package <dir>          assert the required files exist; exit non-zero
         [--summary <file>]       on any miss. Appends a Markdown grid to
                                  --summary (e.g. $GITHUB_STEP_SUMMARY).
"""

from __future__ import annotations

import argparse
import glob
import os
import sys

TARGET_PREFIX = "x86_64-w64-mingw32"

# Default tool — the host-neutral sysroot, the soldr#2336 payload. The
# `mingw-w64-gcc` profile lets the scheduled Windows-host lane reuse the same
# verifier for the full WinLibs bundle (which additionally ships host
# executables under `bin/`).
DEFAULT_TOOL = "mingw-w64-sysroot"

# Per-tool: the catalogue slug plus the files (relative to the extracted
# `package/` root) that together prove a complete, linkable install. Kept in
# step with `mingw_w64_sysroot::verification_paths` /
# `mingw_w64_gcc::verification_paths` on the Rust side. Entries containing a `*`
# are globbed (the gcc runtime nests a per-version subdir).
TOOLS = {
    "mingw-w64-sysroot": {
        "slug": "windows-x64-gnu",
        "required": [
            f"{TARGET_PREFIX}/include/windows.h",
            f"{TARGET_PREFIX}/lib/libkernel32.a",
            f"{TARGET_PREFIX}/lib/libmingw32.a",
            f"{TARGET_PREFIX}/lib/libmsvcrt.a",
            f"{TARGET_PREFIX}/lib/crt2.o",
            f"lib/gcc/{TARGET_PREFIX}/*/libgcc.a",
        ],
    },
    "mingw-w64-gcc": {
        "slug": "windows-x64-gnu",
        "required": [
            "bin/gcc.exe",
            "bin/dlltool.exe",
            "bin/windres.exe",
            f"{TARGET_PREFIX}/include/windows.h",
            f"{TARGET_PREFIX}/lib/libkernel32.a",
        ],
    },
}

# Back-compat module constants (the sysroot profile is what most callers mean).
TOOL = DEFAULT_TOOL
SLUG = TOOLS[DEFAULT_TOOL]["slug"]
REQUIRED_FILES = TOOLS[DEFAULT_TOOL]["required"]


def _tool_spec(tool: str) -> dict:
    if tool not in TOOLS:
        raise SystemExit(f"unknown tool {tool!r}; known: {', '.join(sorted(TOOLS))}")
    return TOOLS[tool]


def locate_package(soldr_home: str, tool: str = DEFAULT_TOOL) -> str:
    """Return the single materialized `package/` directory under a soldr home.

    Mirrors the on-disk layout `syslib_common::ensure_syslib_bundle` writes:
    `<home>/bin/syslib/<tool>/<version>/<slug>/package`.
    """
    slug = _tool_spec(tool)["slug"]
    pattern = os.path.join(soldr_home, "bin", "syslib", tool, "*", slug, "package")
    matches = sorted(p for p in glob.glob(pattern) if os.path.isdir(p))
    if not matches:
        raise SystemExit(
            f"no materialized {tool} sysroot found under {soldr_home!r} "
            f"(looked for {pattern}); did `soldr prepare` run and succeed?"
        )
    if len(matches) > 1:
        raise SystemExit(
            f"ambiguous {tool} sysroot: {len(matches)} matched {pattern}: "
            + ", ".join(matches)
        )
    return matches[0]


def check(package: str, tool: str = DEFAULT_TOOL) -> list[tuple[str, bool, str]]:
    """Resolve every required file against `package`, returning
    (requirement, present, resolved_path) rows."""
    rows: list[tuple[str, bool, str]] = []
    for rel in _tool_spec(tool)["required"]:
        full = os.path.join(package, rel)
        if "*" in rel:
            hits = [p for p in glob.glob(full) if os.path.isfile(p)]
            rows.append((rel, bool(hits), hits[0] if hits else full))
        else:
            rows.append((rel, os.path.isfile(full), full))
    return rows


def render_summary(
    package: str, rows: list[tuple[str, bool, str]], tool: str = DEFAULT_TOOL
) -> str:
    lines = [
        f"## win-gnu `{tool}` smoke",
        "",
        f"Package root: `{package}`",
        "",
        "| Required file | Status |",
        "| --- | --- |",
    ]
    for rel, ok, _ in rows:
        lines.append(f"| `{rel}` | {'✅ present' if ok else '❌ MISSING'} |")
    missing = [rel for rel, ok, _ in rows if not ok]
    lines.append("")
    lines.append(
        "**All required sysroot files present.**"
        if not missing
        else f"**{len(missing)} required file(s) missing — see above.**"
    )
    return "\n".join(lines) + "\n"


def cmd_locate(args: argparse.Namespace) -> int:
    print(locate_package(args.soldr_home, args.tool))
    return 0


def cmd_verify(args: argparse.Namespace) -> int:
    package = args.package
    if not os.path.isdir(package):
        raise SystemExit(f"package dir does not exist: {package!r}")
    rows = check(package, args.tool)
    summary = render_summary(package, rows, args.tool)
    if args.summary:
        with open(args.summary, "a", encoding="utf-8") as fh:
            fh.write(summary)
    print(summary)
    missing = [rel for rel, ok, _ in rows if not ok]
    if missing:
        print(
            "FAIL: missing required sysroot files: " + ", ".join(missing),
            file=sys.stderr,
        )
        return 1
    print("OK: host-neutral win-gnu sysroot is complete.")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_locate = sub.add_parser("locate", help="print the materialized package dir")
    p_locate.add_argument("--soldr-home", required=True)
    p_locate.add_argument("--tool", default=DEFAULT_TOOL, choices=sorted(TOOLS))
    p_locate.set_defaults(func=cmd_locate)

    p_verify = sub.add_parser("verify", help="assert required sysroot files exist")
    p_verify.add_argument("--package", required=True)
    p_verify.add_argument("--tool", default=DEFAULT_TOOL, choices=sorted(TOOLS))
    p_verify.add_argument("--summary", default=None)
    p_verify.set_defaults(func=cmd_verify)

    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
