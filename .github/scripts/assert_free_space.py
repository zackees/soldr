#!/usr/bin/env python3
"""Fail a target-run shard *at the disk*, not three layers above it.

soldr#2933 (phase 1 of soldr#2931). Run 33065541438 exhausted ``C:`` while
extracting a 3,302,138,143-byte nextest archive and the lane reported it as
three unrelated-looking test failures carrying a raw
``Os { code: 112, kind: StorageFull }`` from inside ``isolated_daemon.rs``.
Reading the log top to bottom, the disk was never mentioned: the only
readings taken were ``report_free_space.py --label before/after``, which are
deliberately non-fatal diagnostics (soldr#2734) and print whatever they find
without judging it.

A diagnostic that observes a fatal condition and returns 0 is the reason that
run cost an afternoon of triage. This is the complement: a *guard*. It states
a floor, measures the volume the extraction will actually land on, and exits
non-zero with every number needed to act -- path, volume identity, free
bytes, floor, and the archive size that motivated the floor -- before any
byte is written.

The two are not redundant. ``report_free_space.py`` must never turn a green
run red, so it can never fail. This must fail, so it can never be the thing
that reports routine readings. Keeping them separate is the point.

The floor is the larger of an absolute ``--min-free-gib`` / ``--min-free-bytes``
and, when ``--archive`` and ``--archive-multiple`` are supplied, a multiple of
the compressed archive size. Debug-profile test binaries decompress several
times over, so a floor pinned to the archive scales with the thing that grows.
"""

from __future__ import annotations

import argparse
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

GIB = 1024**3


@dataclass(frozen=True)
class Verdict:
    """The outcome of one headroom check."""

    ok: bool
    message: str


def nearest_existing(path: Path) -> Path | None:
    """The closest ancestor of ``path`` that exists, or None.

    The extraction directory is checked *before* nextest creates it, so the
    path under test routinely does not exist yet. Its capacity is still a
    well-defined question -- it is the capacity of whichever ancestor is
    already there -- and answering it is the entire purpose of a pre-extract
    guard.
    """

    probe = path
    while True:
        try:
            if probe.exists():
                return probe
        except OSError:
            return None
        if probe.parent == probe:
            return None
        probe = probe.parent


def volume_identity(path: Path) -> str:
    """Windows drive letter, or the POSIX mount point ``path`` sits on."""

    drive = Path(path).drive
    if drive:
        return drive
    try:
        probe = Path(path).resolve()
        device = probe.stat().st_dev
    except OSError:
        return str(path)
    while probe.parent != probe:
        try:
            if probe.parent.stat().st_dev != device:
                break
        except OSError:
            break
        probe = probe.parent
    return probe.as_posix()


def archive_size(archive: Path | None) -> int | None:
    if archive is None:
        return None
    try:
        return archive.stat().st_size
    except OSError:
        return None


def required_floor(
    min_free_bytes: int,
    archive_bytes: int | None,
    archive_multiple: float,
) -> int:
    """The larger of the absolute floor and the archive-scaled floor."""

    if archive_bytes is None or archive_multiple <= 0:
        return min_free_bytes
    return max(min_free_bytes, int(archive_bytes * archive_multiple))


def _gib(value: int) -> str:
    return f"{value / GIB:.2f} GiB ({value} bytes)"


def evaluate(
    path: Path,
    floor_bytes: int,
    *,
    label: str = "",
    archive: Path | None = None,
    archive_bytes: int | None = None,
) -> Verdict:
    """Measure ``path``'s volume against ``floor_bytes``.

    An unmeasurable path is a failure, not a pass. This runs on a lane whose
    known failure mode is a full disk, and a full disk is exactly when
    ``disk_usage`` is most likely to misbehave -- treating "cannot tell" as
    "fine" would reproduce the original silence.
    """

    when = f" [{label}]" if label else ""
    measured = nearest_existing(path)
    if measured is None:
        return Verdict(
            ok=False,
            message=(
                f"FATAL{when}: no existing ancestor of {path.as_posix()} could "
                "be measured, so the free space on the extraction volume is "
                "unknown. soldr#2933: this lane fails on disk; an unmeasurable "
                "volume is treated as a failure, never as a pass."
            ),
        )
    try:
        usage = shutil.disk_usage(measured)
    except OSError as error:
        return Verdict(
            ok=False,
            message=(
                f"FATAL{when}: could not read free space for "
                f"{measured.as_posix()} ({error}). soldr#2933: an unreadable "
                "volume is treated as a failure."
            ),
        )

    details = [
        f"  extraction path : {path.as_posix()}",
        f"  measured on     : {measured.as_posix()}",
        f"  volume          : {volume_identity(measured)}",
        f"  free            : {_gib(usage.free)}",
        f"  total           : {_gib(usage.total)}",
        f"  required floor  : {_gib(floor_bytes)}",
    ]
    if archive is not None and archive_bytes is not None:
        details.append(f"  archive         : {archive.as_posix()} {_gib(archive_bytes)}")
    elif archive is not None:
        details.append(f"  archive         : {archive.as_posix()} (unreadable)")
    body = "\n".join(details)

    if usage.free >= floor_bytes:
        return Verdict(ok=True, message=f"target-run disk floor OK{when}\n{body}")
    return Verdict(
        ok=False,
        message=(
            f"FATAL{when}: insufficient free disk for the nextest archive "
            f"extraction\n{body}\n"
            "soldr#2933: a shard that starts below this floor does not fail "
            "here -- it fails minutes later as `Os { code: 112, "
            "kind: StorageFull }` inside whichever test happened to be "
            "writing at the time (run 33065541438). Either the extraction "
            "volume was chosen wrongly (see select_extract_volume.py, which "
            "should have picked the largest volume) or the archive has "
            "outgrown the runner and soldr#2931 phase 2 is due."
        ),
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--path",
        type=Path,
        required=True,
        help="Directory the archive is (or was) extracted into.",
    )
    parser.add_argument(
        "--label",
        default="",
        help="Marks when the check ran, e.g. 'pre-extract' or 'post-extract'.",
    )
    parser.add_argument(
        "--min-free-gib",
        type=float,
        default=0.0,
        help="Absolute floor in GiB.",
    )
    parser.add_argument(
        "--min-free-bytes",
        type=int,
        default=0,
        help="Absolute floor in bytes; combined with --min-free-gib by max().",
    )
    parser.add_argument(
        "--archive",
        type=Path,
        default=None,
        help="Compressed archive whose size scales the floor and is reported.",
    )
    parser.add_argument(
        "--archive-multiple",
        type=float,
        default=0.0,
        help=(
            "Floor multiplier applied to the archive size. Debug test "
            "binaries decompress several times over, so the floor tracks the "
            "archive rather than a constant that silently stops being enough."
        ),
    )
    args = parser.parse_args(argv)

    absolute = max(int(args.min_free_gib * GIB), args.min_free_bytes)
    measured_archive = archive_size(args.archive)
    floor = required_floor(absolute, measured_archive, args.archive_multiple)

    verdict = evaluate(
        args.path,
        floor,
        label=args.label,
        archive=args.archive,
        archive_bytes=measured_archive,
    )
    if verdict.ok:
        print(verdict.message)
        return 0
    print(verdict.message, file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
