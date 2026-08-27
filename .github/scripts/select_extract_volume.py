#!/usr/bin/env python3
"""Choose, explicitly, the volume a target-run shard extracts its archive onto.

soldr#2933 (phase 1 of soldr#2931). Run 33065541438 killed
``target-run x86_64-pc-windows-gnu (1-of-3)`` with a raw
``Os { code: 112, kind: StorageFull }``. The postmortem numbers:

* the nextest archive is 3,302,138,143 bytes compressed;
* ``cargo-nextest`` decompressed it into
  ``C:\\Users\\runneradmin\\AppData\\Local\\Temp\\nextest-archive-*`` -- the
  *OS temp* volume, chosen implicitly because nothing told it otherwise;
* ``C:`` began the job with 31.03 GiB free and ran out;
* ``D:`` held 143.61 GiB free for the entire job and was never touched.

The lane did not run out of disk. It ran out of *the wrong disk*. Nothing in
the workflow ever named a volume, so the extraction inherited whatever
``GetTempPath`` returned, and the four-times-larger volume sat idle beside it.

This script removes the implicit choice. It probes every candidate volume,
picks the one with the most free space, creates an extraction root there, and
writes the resulting paths to ``$GITHUB_ENV`` so the replay steps -- and,
with ``--redirect-temp``, every test sandbox built under ``TMP``/``TEMP`` --
land on the volume that can actually hold them.

Deliberately not a soldr concern: soldr's own watchdog is pinned at 1 GiB
warn/block on this lane (soldr#2699) so it never prunes mid-run. By the time
soldr would speak, the runner is already gone. Volume selection has to happen
before a single byte is written.

POSIX runners are unaffected in practice: their candidates all resolve to the
same volume, so the "largest" volume is the one they were already using and
``--redirect-temp`` becomes a no-op. The script is still run there so the
chosen path is *stated* in the log on every platform rather than inferred.
"""

from __future__ import annotations

import argparse
import os
import shutil
import string
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

GIB = 1024**3

# Removable and optical drives report tiny (or zero) totals and would win a
# "most free space" contest by accident on an empty reading. Nothing smaller
# than this is a plausible home for a multi-gigabyte extraction.
MIN_PLAUSIBLE_TOTAL_BYTES = 4 * GIB


@dataclass(frozen=True)
class Volume:
    """A candidate extraction volume and its measured capacity."""

    root: Path
    identity: str
    free: int
    total: int

    def describe(self) -> str:
        return (
            f"{self.identity} root={self.root.as_posix()} "
            f"free={self.free / GIB:.2f}GiB total={self.total / GIB:.2f}GiB"
        )


def volume_identity(path: Path) -> str:
    """A short, stable name for the volume ``path`` lives on.

    Windows gets the drive letter (``D:``) because that is the identity the
    failure was reported in. POSIX gets the mount point, found by walking up
    until ``st_dev`` changes, which is the equivalent statement there.
    """

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


def probe(root: Path, min_total: int = MIN_PLAUSIBLE_TOTAL_BYTES) -> Volume | None:
    """Measure ``root``, or return None when it is not a usable candidate.

    Returns None rather than raising for every failure mode -- a drive letter
    that exists but has no medium, a network share that is offline, a mount
    that refuses ``statvfs``. A candidate that cannot be measured is simply
    not a candidate; it must never abort the selection of the others.

    ``min_total`` is dropped to zero for an explicit ``--root``: the
    plausibility filter exists to keep an empty optical drive from winning a
    "most free space" contest by accident, and an operator who names a path
    has already answered that question.
    """

    try:
        if not root.is_dir():
            return None
        usage = shutil.disk_usage(root)
    except OSError:
        return None
    if usage.total < min_total:
        return None
    return Volume(
        root=root, identity=volume_identity(root), free=usage.free, total=usage.total
    )


def windows_drive_roots() -> list[Path]:
    """Every drive root Windows will admit to having.

    ``os.listdrives`` (3.12+) is preferred because probing letters one by one
    can spin up removable media. The letter sweep is the fallback for older
    interpreters and is only reached off the runner.
    """

    lister = getattr(os, "listdrives", None)
    if lister is not None:
        try:
            return [Path(drive) for drive in lister()]
        except OSError:
            pass
    roots: list[Path] = []
    for letter in string.ascii_uppercase:
        candidate = Path(f"{letter}:\\")
        try:
            if candidate.is_dir():
                roots.append(candidate)
        except OSError:
            continue
    return roots


def candidate_roots(
    workspace: Path,
    runner_temp: Path | None,
    override: Path | None,
    *,
    windows: bool,
) -> list[Path]:
    """The volumes worth considering, most-specific first.

    An explicit override short-circuits everything: an operator who names a
    path has already made the decision this script exists to make.
    """

    if override is not None:
        return [override]
    if windows:
        return windows_drive_roots()
    # Deliberately not `/`. On POSIX runners these three already resolve to
    # the volume that matters, and probing `/` for writability would create a
    # stray top-level directory in any container that happens to run as root
    # -- a side effect with no upside, since `/` is the same volume anyway.
    roots = [runner_temp, workspace, Path(tempfile.gettempdir())]
    seen: list[Path] = []
    for root in roots:
        if root is None:
            continue
        if root not in seen:
            seen.append(root)
    return seen


def choose(volumes: list[Volume]) -> Volume | None:
    """The roomiest volume, ties broken by identity so runs are reproducible."""

    if not volumes:
        return None
    return sorted(volumes, key=lambda volume: (-volume.free, volume.identity))[0]


def is_writable(root: Path, probe_name: str) -> bool:
    """Whether a directory can actually be created under ``root``.

    ``D:`` existing says nothing about whether this process may write to its
    root. A volume that measures large but rejects the ``mkdir`` is worse
    than one that was never considered, because the failure would surface
    inside nextest rather than here.
    """

    candidate = root / probe_name
    try:
        candidate.mkdir(parents=True, exist_ok=True)
    except OSError:
        return False
    return True


def env_lines(
    extract_root: Path,
    extract_dir: Path,
    temp_dir: Path | None,
) -> list[str]:
    """The ``$GITHUB_ENV`` assignments this selection implies.

    Forward slashes on purpose: these values are consumed by ``shell: bash``
    steps on Windows runners, and MSYS leaves ``D:/x`` alone while it is free
    to rewrite other spellings. Native Windows binaries accept them.
    """

    lines = [
        f"NEXTEST_EXTRACT_ROOT={extract_root.as_posix()}",
        f"NEXTEST_EXTRACT_DIR={extract_dir.as_posix()}",
    ]
    if temp_dir is not None:
        lines.extend(
            [
                f"TMP={temp_dir.as_posix()}",
                f"TEMP={temp_dir.as_posix()}",
                f"TMPDIR={temp_dir.as_posix()}",
            ]
        )
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--workspace",
        type=Path,
        default=Path.cwd(),
        help="Checkout root; one of the POSIX candidates.",
    )
    parser.add_argument(
        "--runner-temp",
        type=Path,
        default=None,
        help="$RUNNER_TEMP, when the caller has one.",
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=None,
        help="Explicit extraction volume/directory; skips volume selection.",
    )
    parser.add_argument(
        "--prefix",
        default="soldr-ci",
        help="Directory created on the chosen volume to hold everything.",
    )
    parser.add_argument(
        "--name",
        default="nextest-archive",
        help="Extraction directory name under the prefix.",
    )
    parser.add_argument(
        "--redirect-temp",
        action="store_true",
        help=(
            "Also point TMP/TEMP/TMPDIR at the chosen volume when it differs "
            "from the current temp volume. soldr#2933: the isolated-home "
            "tests build their sandboxes in temp, and that is the allocation "
            "that exhausted C:."
        ),
    )
    parser.add_argument(
        "--github-env",
        type=Path,
        default=None,
        help="Path to $GITHUB_ENV; assignments are appended when given.",
    )
    args = parser.parse_args(argv)

    windows = os.name == "nt"
    roots = candidate_roots(args.workspace, args.runner_temp, args.root, windows=windows)
    min_total = 0 if args.root is not None else MIN_PLAUSIBLE_TOTAL_BYTES
    measured = [v for v in (probe(root, min_total) for root in roots) if v]
    for volume in sorted(measured, key=lambda item: item.identity):
        print(f"target-run volume candidate: {volume.describe()}")

    writable = [v for v in measured if is_writable(v.root, args.prefix)]
    chosen = choose(writable)
    if chosen is None:
        print(
            "FATAL: no writable volume could be measured for the nextest "
            "archive extraction. Candidates considered: "
            + ", ".join(root.as_posix() for root in roots),
            file=sys.stderr,
        )
        return 1

    extract_root = chosen.root / args.prefix
    extract_dir = extract_root / args.name
    extract_root.mkdir(parents=True, exist_ok=True)

    # `--extract-to` must name a directory that already EXISTS: nextest
    # canonicalizes the destination before it writes, so an absent path fails
    # the extraction outright with
    #   error canonicalizing destination directory `<dir>`
    #   No such file or directory (os error 2)
    # rather than being created on demand. It must also be empty -- handing
    # nextest content left by a previous attempt is how an extraction turns
    # into a half-merged tree -- so the slot is cleared and then recreated.
    if extract_dir.exists():
        shutil.rmtree(extract_dir, ignore_errors=True)
    extract_dir.mkdir(parents=True, exist_ok=True)

    temp_dir: Path | None = None
    if args.redirect_temp:
        current_temp = Path(tempfile.gettempdir())
        if volume_identity(current_temp) != chosen.identity:
            temp_dir = extract_root / "tmp"
            temp_dir.mkdir(parents=True, exist_ok=True)
        else:
            print(
                "target-run volume: temp already lives on the chosen volume "
                f"({chosen.identity}); TMP/TEMP left alone"
            )

    print(f"target-run volume: chose {chosen.describe()}")
    print(f"target-run volume: extraction dir {extract_dir.as_posix()}")
    if temp_dir is not None:
        print(f"target-run volume: temp redirected to {temp_dir.as_posix()}")

    lines = env_lines(extract_root, extract_dir, temp_dir)
    for line in lines:
        print(f"target-run volume env: {line}")
    if args.github_env is not None:
        with args.github_env.open("a", encoding="utf-8") as stream:
            for line in lines:
                stream.write(line + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
