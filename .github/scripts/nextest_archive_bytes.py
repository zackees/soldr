#!/usr/bin/env python3
"""Attribute the bytes in an extracted nextest archive to individual binaries.

soldr#2931 wants the RED evidence for *why* the target-run archive reached
3,302,138,143 bytes compressed, and soldr#2933 is where that evidence gets
collected. Until now the only number anyone had was the size of the tarball,
which is compatible with every explanation and therefore supports none:

* one enormous binary, or hundreds of medium ones;
* duplicated static linking -- every integration test relinking the whole
  workspace, so the same code ships once per test binary;
* symbol tables and debug info, which dominate debug-profile Rust binaries
  and compress well but extract enormously.

Those call for completely different fixes (split the workspace, share a
dylib, strip/split debug info, cut the number of test targets), so guessing
is expensive. This prints the distribution: every binary with its size,
sorted descending, plus the totals and the count of test binaries.

**This is a diagnostic and must never fail the lane.** Every failure mode
returns 0 with a note. The archive being unreadable is a real answer -- it
just is not one worth turning a run red for, and the guard that *is* allowed
to do that is ``assert_free_space.py``.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

GIB = 1024**3
MIB = 1024**2

# Extensions that are build byproducts rather than the executables nextest
# will run. Counting them as "test binaries" would inflate the count without
# changing the byte story, and the byte story is the point.
BYPRODUCT_SUFFIXES = frozenset(
    {
        ".d",
        ".rlib",
        ".rmeta",
        ".o",
        ".a",
        ".pdb",
        ".json",
        ".txt",
        ".toml",
        ".lock",
    }
)

EXECUTABLE_SUFFIXES = frozenset({"", ".exe"})


@dataclass(frozen=True)
class Entry:
    """One file in the extracted tree."""

    path: str
    size: int
    is_test_binary: bool


def is_test_binary(relative: Path) -> bool:
    """Whether ``relative`` looks like a nextest-runnable test executable.

    Cargo puts test executables in ``<target>/<profile>/deps/``. The
    ``deps/`` requirement is what separates them from the handful of other
    executables an archive carries (build-script outputs, the packaged
    ``soldr`` binaries), which are not what the byte question is about.
    """

    if "deps" not in relative.parts:
        return False
    suffix = relative.suffix.lower()
    if suffix in BYPRODUCT_SUFFIXES:
        return False
    return suffix in EXECUTABLE_SUFFIXES


def scan(root: Path) -> list[Entry]:
    """Every file under ``root``, with the test binaries flagged.

    Uses ``os.walk`` rather than ``rglob`` so an unreadable subdirectory on a
    disk that may itself be the problem is skipped instead of aborting the
    whole scan.
    """

    entries: list[Entry] = []
    for directory, _subdirs, files in os.walk(root, onerror=lambda _error: None):
        base = Path(directory)
        for name in files:
            candidate = base / name
            try:
                size = candidate.stat().st_size
            except OSError:
                continue
            try:
                relative = candidate.relative_to(root)
            except ValueError:
                relative = candidate
            entries.append(
                Entry(
                    path=relative.as_posix(),
                    size=size,
                    is_test_binary=is_test_binary(relative),
                )
            )
    return entries


def summarize(entries: list[Entry], archive_bytes: int | None) -> dict:
    """Totals plus the descending per-binary breakdown."""

    binaries = sorted(
        (entry for entry in entries if entry.is_test_binary),
        key=lambda entry: (-entry.size, entry.path),
    )
    everything = sorted(entries, key=lambda entry: (-entry.size, entry.path))
    extracted_total = sum(entry.size for entry in entries)
    binary_total = sum(entry.size for entry in binaries)
    ratio: float | None = None
    if archive_bytes:
        ratio = round(extracted_total / archive_bytes, 2)
    return {
        "schema_version": 1,
        "archive_bytes": archive_bytes,
        "extracted_bytes": extracted_total,
        "extracted_over_archive": ratio,
        "file_count": len(entries),
        "test_binary_count": len(binaries),
        "test_binary_bytes": binary_total,
        "test_binary_share": (
            round(binary_total / extracted_total, 4) if extracted_total else None
        ),
        "test_binaries": [asdict(entry) for entry in binaries],
        "largest_files": [asdict(entry) for entry in everything[:50]],
    }


def _mib(value: int) -> str:
    return f"{value / MIB:,.1f}"


def render_markdown(summary: dict, top: int) -> str:
    """A ``$GITHUB_STEP_SUMMARY`` block naming where the bytes went."""

    lines = [
        "",
        "### Nextest archive byte attribution (soldr#2931)",
        "",
    ]
    archive = summary["archive_bytes"]
    extracted = summary["extracted_bytes"]
    lines.append("| Measure | Value |")
    lines.append("| --- | ---: |")
    lines.append(
        "| Archive (compressed) | "
        + (f"{archive / GIB:.2f} GiB" if archive else "n/a")
        + " |"
    )
    lines.append(f"| Extracted total | {extracted / GIB:.2f} GiB |")
    if summary["extracted_over_archive"] is not None:
        lines.append(
            f"| Extracted / archive | {summary['extracted_over_archive']}x |"
        )
    lines.append(f"| Files extracted | {summary['file_count']} |")
    lines.append(f"| Test binaries | {summary['test_binary_count']} |")
    lines.append(
        f"| Test binary bytes | {summary['test_binary_bytes'] / GIB:.2f} GiB |"
    )
    if summary["test_binary_share"] is not None:
        lines.append(
            f"| Test binary share | {summary['test_binary_share'] * 100:.1f}% |"
        )

    binaries = summary["test_binaries"][:top]
    if binaries:
        lines.extend(
            [
                "",
                f"<details><summary>Largest {len(binaries)} test binaries</summary>",
                "",
                "| MiB | Binary |",
                "| ---: | --- |",
            ]
        )
        for entry in binaries:
            lines.append(f"| {_mib(entry['size'])} | `{entry['path']}` |")
        lines.extend(["", "</details>", ""])
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--extract-dir",
        default="",
        help="Directory the nextest archive was extracted into.",
    )
    parser.add_argument(
        "--archive",
        default="",
        help="The compressed archive, for the compression ratio.",
    )
    parser.add_argument("--top", type=int, default=30)
    parser.add_argument(
        "--json", type=Path, default=None, help="Write the full report here."
    )
    parser.add_argument(
        "--summary",
        type=Path,
        default=None,
        help="Append the markdown block to this file ($GITHUB_STEP_SUMMARY).",
    )
    args = parser.parse_args(argv)

    if not args.extract_dir:
        print("archive bytes: no extraction directory given; nothing to attribute")
        return 0
    root = Path(args.extract_dir)
    if not root.is_dir():
        print(f"archive bytes: {root.as_posix()} is not a directory; skipping")
        return 0

    archive_bytes: int | None = None
    if args.archive:
        try:
            archive_bytes = Path(args.archive).stat().st_size
        except OSError:
            archive_bytes = None

    # OSError only, deliberately. A diagnostic must not fail the lane, but a
    # blanket `except Exception` would also swallow a genuine bug in this
    # script and report it as "no attribution available" forever. Walking a
    # tree on a disk that may itself be the problem raises OSError; anything
    # else here is a defect and should be seen.
    try:
        summary = summarize(scan(root), archive_bytes)
    except OSError as error:
        print(f"archive bytes: attribution failed ({error}); skipping")
        return 0

    print(
        "archive bytes: "
        f"extracted={summary['extracted_bytes'] / GIB:.2f}GiB "
        f"files={summary['file_count']} "
        f"test_binaries={summary['test_binary_count']} "
        f"test_binary_bytes={summary['test_binary_bytes'] / GIB:.2f}GiB"
    )
    for entry in summary["test_binaries"][: args.top]:
        print(f"archive bytes: {_mib(entry['size'])} MiB  {entry['path']}")

    if args.json is not None:
        try:
            args.json.parent.mkdir(parents=True, exist_ok=True)
            args.json.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        except OSError as error:
            print(f"archive bytes: could not write {args.json} ({error})")
    if args.summary is not None:
        try:
            with args.summary.open("a", encoding="utf-8") as stream:
                stream.write(render_markdown(summary, args.top))
        except OSError as error:
            print(f"archive bytes: could not append to {args.summary} ({error})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
