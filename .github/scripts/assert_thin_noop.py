#!/usr/bin/env python3
"""Assert Cargo correctness after a thin-v2 cache restore.

This script implements the verification gate described in
``docs/THIN_TARGET_CACHE_PRUNING.md`` (Section 5).

The "second build is a no-op" claim is what proves the thin-v2 slice carries
enough state for cargo to skip work. If a thin-v2 restore is missing some bit
of fingerprint state, cargo will print
``Compiling <crate>`` lines on the second build instead of just
``Finished`` — that is the signal we look for.

Why parse text instead of cargo ``--timings=json``:

- Plain ``cargo build`` stderr is reliable across all toolchains we ship,
  with no nightly flag dependency.
- The output is small (KB, not MB), so checking it in CI is cheap.
- The signal is unambiguous: a fresh build prints ``Compiling foo v1.2.3``
  per unit; a fully-fresh restore prints only one ``Finished ...`` line.

By default the script verifies a complete restore is a near no-op. With
``--expect-incomplete-restore``, it instead requires Cargo to rebuild a
first-party unit when thin-v2 intentionally omitted its primary outputs.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# Cargo's status lines look like:
#   "   Compiling soldr-cli v0.7.11 (/path/to/crate)"
#   "   Compiling serde v1.0.219"
#   "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.12s"
# The leading whitespace and tag are stable across cargo versions back to 1.40.
_COMPILING_LINE = re.compile(
    r"^\s*Compiling\s+(?P<name>\S+)\s+v(?P<version>\S+)(?:\s+\((?P<path>[^)]+)\))?\s*$"
)
# Cargo JSON messages can be interleaved with the human status stream without
# a newline (for example ``Finished{"reason":"build-finished"}``). Accept
# both the normal text form and that transparent tee'd form.
_FINISHED_LINE = re.compile(r"(?<!\w)Finished(?:\s+|(?=\{))")
_FRESH_LINE = re.compile(r"^\s*Fresh\s+(?P<name>\S+)\s+v(?P<version>\S+)")


@dataclass(frozen=True)
class CompilingUnit:
    name: str
    version: str
    path: str | None  # path is present when the unit is a path-dep (workspace member)

    @property
    def is_path_dep(self) -> bool:
        """A unit is "first-party" if cargo logged a path for it.

        Path-dep units include workspace members and ``[patch.*]`` overrides.
        Registry crates do not get a path suffix. We treat path-deps as the
        primary signal because workspace members are what users edit and what
        we want to gate on for thin-v2 correctness.
        """
        return self.path is not None


@dataclass
class BuildLogSummary:
    finished_seen: bool
    compiling_units: list[CompilingUnit]
    fresh_count: int
    raw_lines: int

    @property
    def first_party_compiles(self) -> list[CompilingUnit]:
        return [u for u in self.compiling_units if u.is_path_dep]

    @property
    def third_party_compiles(self) -> list[CompilingUnit]:
        return [u for u in self.compiling_units if not u.is_path_dep]


def parse_build_log(text: str) -> BuildLogSummary:
    """Parse a captured ``cargo build`` log (stdout+stderr) into a summary."""
    finished = False
    compiling: list[CompilingUnit] = []
    fresh = 0
    raw = 0
    for line in text.splitlines():
        raw += 1
        m = _COMPILING_LINE.match(line)
        if m:
            compiling.append(
                CompilingUnit(
                    name=m.group("name"),
                    version=m.group("version"),
                    path=m.group("path"),
                )
            )
            continue
        if _FRESH_LINE.match(line):
            fresh += 1
            continue
        if _FINISHED_LINE.search(line):
            finished = True
    return BuildLogSummary(
        finished_seen=finished,
        compiling_units=compiling,
        fresh_count=fresh,
        raw_lines=raw,
    )


def assert_second_build_is_noop(
    first_log: str,
    second_log: str,
    *,
    tolerance: int = 2,
    require_first_built_something: bool = True,
    allow_empty_second: bool = False,
) -> tuple[BuildLogSummary, BuildLogSummary, list[str]]:
    """Validate that the second build was a near no-op.

    Returns a tuple of ``(first_summary, second_summary, errors)``.
    ``errors`` is empty on success.
    """
    first = parse_build_log(first_log)
    second = parse_build_log(second_log)
    errors: list[str] = []

    if require_first_built_something and not first.compiling_units:
        errors.append(
            "first build did not show any Compiling lines; the verifier "
            "expected a cold build as the baseline. "
            "If this is intentional, pass --allow-empty-first."
        )

    second_empty_allowed = allow_empty_second and second.raw_lines == 0
    if not second.finished_seen and not second_empty_allowed:
        errors.append(
            "second build did not produce a 'Finished' line; the build "
            "likely failed or was truncated."
        )

    # First-party (workspace + patched) compiles are the strict gate. Even one
    # of these on the second build means thin-v2 missed a fingerprint that the
    # workspace owns.
    fp_compiles = second.first_party_compiles
    if len(fp_compiles) > 0:
        names = ", ".join(f"{u.name}@{u.version}" for u in fp_compiles)
        errors.append(
            f"second build re-compiled {len(fp_compiles)} first-party unit(s): {names}. "
            "Thin-v2 must preserve enough fingerprint state to skip workspace crates."
        )

    # Third-party compiles past the tolerance are softer but still worth
    # surfacing — they usually mean a build script re-ran or a proc-macro got
    # rebuilt because its fingerprint output was dropped.
    tp_compiles = second.third_party_compiles
    if len(tp_compiles) > tolerance:
        names = ", ".join(f"{u.name}@{u.version}" for u in tp_compiles[:10])
        suffix = "" if len(tp_compiles) <= 10 else f" (+{len(tp_compiles) - 10} more)"
        errors.append(
            f"second build re-compiled {len(tp_compiles)} third-party unit(s), "
            f"tolerance is {tolerance}. Examples: {names}{suffix}."
        )

    return first, second, errors


def assert_incomplete_restore_rebuilds(
    first_log: str,
    second_log: str,
) -> tuple[BuildLogSummary, BuildLogSummary, list[str]]:
    """Verify Cargo stays authoritative when primary outputs were not restored."""
    first = parse_build_log(first_log)
    second = parse_build_log(second_log)
    errors: list[str] = []

    if not first.compiling_units:
        errors.append(
            "first build did not show any Compiling lines; the verifier "
            "expected a cold build as the baseline."
        )
    if not second.finished_seen:
        errors.append(
            "fresh-target build did not produce a 'Finished' line; the build "
            "likely failed or was truncated."
        )
    if not second.first_party_compiles:
        errors.append(
            "fresh-target restore did not rebuild a first-party unit even though "
            "thin-v2 omits required rlib/rmeta outputs; refusing to claim Cargo Fresh."
        )

    return first, second, errors


def _format_summary(label: str, summary: BuildLogSummary) -> str:
    return (
        f"{label}: compiling={len(summary.compiling_units)} "
        f"(first_party={len(summary.first_party_compiles)}, "
        f"third_party={len(summary.third_party_compiles)}), "
        f"fresh={summary.fresh_count}, finished={summary.finished_seen}, "
        f"raw_lines={summary.raw_lines}"
    )


def _build_argparser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Verify that a thin-v2 cache restore makes the second cargo build "
            "a no-op. Reads two captured cargo build logs."
        ),
    )
    parser.add_argument(
        "first_log",
        type=Path,
        help="Path to the first (cold) cargo build log (stdout+stderr capture).",
    )
    parser.add_argument(
        "second_log",
        type=Path,
        help="Path to the second (warm) cargo build log (stdout+stderr capture).",
    )
    parser.add_argument(
        "--tolerance",
        type=int,
        default=2,
        help=(
            "Maximum number of third-party compile units allowed in the "
            "second build. First-party (workspace) compiles are never "
            "tolerated. Default: 2."
        ),
    )
    parser.add_argument(
        "--allow-empty-first",
        action="store_true",
        help=(
            "Skip the sanity check that the first build performed work. "
            "Useful when the first build was itself partly cached."
        ),
    )
    parser.add_argument(
        "--allow-empty-second",
        action="store_true",
        help=(
            "Accept an empty second log as a successful no-op. Use only when "
            "the calling step already fails if the second build command fails."
        ),
    )
    parser.add_argument(
        "--expect-incomplete-restore",
        action="store_true",
        help=(
            "Require a first-party rebuild after restoring a slice that omits "
            "primary outputs. This guards against falsely claiming Cargo Fresh."
        ),
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _build_argparser().parse_args(argv)
    try:
        first_text = args.first_log.read_text(encoding="utf-8", errors="replace")
        second_text = args.second_log.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError as exc:
        print(f"assert_thin_noop: input log not found: {exc}", file=sys.stderr)
        return 2

    if args.expect_incomplete_restore:
        first, second, errors = assert_incomplete_restore_rebuilds(first_text, second_text)
    else:
        first, second, errors = assert_second_build_is_noop(
            first_text,
            second_text,
            tolerance=args.tolerance,
            require_first_built_something=not args.allow_empty_first,
            allow_empty_second=args.allow_empty_second,
        )

    print(_format_summary("first ", first))
    print(_format_summary("second", second))

    if errors:
        print("", file=sys.stderr)
        print("assert_thin_noop: FAIL", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    if args.expect_incomplete_restore:
        print("assert_thin_noop: OK (Cargo correctly rebuilt missing primary outputs)")
    else:
        print("assert_thin_noop: OK (second build is a no-op within tolerance)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
