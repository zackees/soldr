#!/usr/bin/env python3
"""Ratchet the minimum macOS version the Darwin release binaries demand.

The Linux side of soldr#1060 is well guarded now: musl artifacts are checked
for static linking, gnu artifacts have a glibc-floor ratchet. macOS has the
identical failure mode and nothing watching it.

A Mach-O records the oldest OS it will start on (`LC_BUILD_VERSION.minos`, or
`LC_VERSION_MIN_MACOSX` on older tooling). If that value drifts upward, the
binary still builds, still passes every test on the CI image, and simply
refuses to launch on older Macs -- the same shape as the GLIBC_2.39 problem,
discovered in the same place: on a user's machine.

Nothing in this repo sets `MACOSX_DEPLOYMENT_TARGET`, so the value is whatever
the toolchain and the pinned Apple SDK happen to produce. Measured on the
published v0.8.29 artifacts:

    aarch64  soldr, soldr-daemon, crgx, cargo-chef  minos 11.0  (sdk 15.5)
    x86_64   soldr                                  minos 11.0  (sdk 15.5)
    x86_64   crgx                                   minos 10.12 (sdk 15.5)

Two things worth noting there. 11.0 is correct and unavoidable for Apple
Silicon, but on x86_64 it is *higher* than Rust's 10.12 default for that
target -- so soldr asks for more than it was asked to. And the bundle is not
even internally consistent: crgx targets 10.12 while soldr next to it targets
11.0. Neither was a decision; both are defaults nobody pinned.

This does not change any of that. It records the current values so a future
SDK or toolchain bump cannot raise them unnoticed.

Usage:
    python3 .github/scripts/verify_macos_min_version.py --max-min-os 11.0 <binary>...

Exit codes:
  0 - every binary's minimum is at or below the ceiling
  1 - one is higher, or could not be determined
"""

from __future__ import annotations

import argparse
import struct
import sys
from pathlib import Path

MH_MAGIC_64 = 0xFEEDFACF  # little-endian 64-bit
MH_CIGAM_64 = 0xCFFAEDFE  # byte-swapped 64-bit
# Universal ("fat") binaries wrap several thin ones. We do not walk them, but
# we must recognise them so they fail as "cannot verify" instead of silently
# parsing as garbage.
FAT_MAGICS = {0xCAFEBABE, 0xBEBAFECA, 0xCAFEBABF, 0xBFBAFECA}

LC_VERSION_MIN_MACOSX = 0x24
LC_BUILD_VERSION = 0x32


class MachOError(Exception):
    """The file could not be understood. Never a pass."""


def decode_version(value: int) -> "tuple[int, int, int]":
    """Mach-O packs X.Y.Z as nibbles: xxxx.yy.zz."""
    return ((value >> 16) & 0xFFFF, (value >> 8) & 0xFF, value & 0xFF)


def format_version(version: "tuple[int, ...]") -> str:
    return ".".join(str(part) for part in version)


def parse_version(text: str) -> "tuple[int, ...]":
    parts = [p for p in text.strip().split(".") if p != ""]
    return tuple(int(p) for p in parts if p.isdigit())


def _padded(version: "tuple[int, ...]") -> "tuple[int, int, int]":
    """Compare 11.0 and 11.0.0 as equal.

    Tuple comparison alone makes `(11, 0) < (11, 0, 0)`, so a ceiling written
    "11.0" would reject a binary whose minimum is exactly 11.0.0 -- failing
    the release for matching the ceiling it was given.
    """
    parts = tuple(version) + (0, 0, 0)
    return parts[0], parts[1], parts[2]


def minimum_os(data: bytes) -> "tuple[int, int, int]":
    """The oldest macOS this Mach-O will start on.

    Raises MachOError rather than returning a default, because every default
    is wrong: a low one waves the binary through, a high one fails the release
    for no reason.
    """
    if len(data) < 32:
        raise MachOError("file is too short to be a Mach-O")
    (magic,) = struct.unpack("<I", data[:4])
    if magic in FAT_MAGICS:
        raise MachOError("universal (fat) binary; inspect each slice separately")
    if magic not in (MH_MAGIC_64, MH_CIGAM_64):
        raise MachOError(f"not a 64-bit Mach-O (magic {magic:#x})")

    endian = "<" if magic == MH_MAGIC_64 else ">"
    (ncmds,) = struct.unpack(endian + "I", data[16:20])

    offset = 32
    for _ in range(ncmds):
        if offset + 8 > len(data):
            raise MachOError("load commands run past the end of the file")
        cmd, size = struct.unpack(endian + "II", data[offset : offset + 8])
        if size < 8:
            raise MachOError("load command with an impossible size")
        if cmd == LC_BUILD_VERSION and offset + 16 <= len(data):
            (minos,) = struct.unpack(endian + "I", data[offset + 12 : offset + 16])
            return decode_version(minos)
        if cmd == LC_VERSION_MIN_MACOSX and offset + 12 <= len(data):
            (minos,) = struct.unpack(endian + "I", data[offset + 8 : offset + 12])
            return decode_version(minos)
        offset += size

    # A Mach-O with no minimum-OS command tells us nothing, and "nothing" must
    # not read as "fine".
    raise MachOError("no LC_BUILD_VERSION or LC_VERSION_MIN_MACOSX load command")


def main(argv: "list[str] | None" = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+", help="Mach-O binaries to inspect")
    parser.add_argument(
        "--max-min-os",
        default="11.0",
        help=(
            "Highest minimum-macOS a binary may demand. Defaults to the "
            "currently measured 11.0; this is a ratchet, so raising it should "
            "be a deliberate decision with a note, not a reflex."
        ),
    )
    args = parser.parse_args(argv)

    ceiling = parse_version(args.max_min_os)
    if not ceiling:
        print(
            f"verify_macos_min_version: --max-min-os {args.max_min_os!r} "
            f"is not a version",
            file=sys.stderr,
        )
        return 1

    failures = 0
    for name in args.binaries:
        path = Path(name)
        try:
            data = path.read_bytes()
        except OSError as error:
            print(
                f"verify_macos_min_version: cannot read {name}: {error}",
                file=sys.stderr,
            )
            failures += 1
            continue

        try:
            minos = minimum_os(data)
        except MachOError as error:
            print(
                f"verify_macos_min_version: cannot determine minimum OS for "
                f"{name}: {error}",
                file=sys.stderr,
            )
            failures += 1
            continue

        if _padded(minos) <= _padded(ceiling):
            print(
                f"verify_macos_min_version: {name} runs on macOS "
                f"{format_version(minos)}+ (ceiling {format_version(ceiling)}) - OK"
            )
            continue

        failures += 1
        print(
            f"verify_macos_min_version: {name} demands macOS "
            f"{format_version(minos)}+, above the {format_version(ceiling)} "
            f"ceiling (soldr#1060).",
            file=sys.stderr,
        )
        print(
            "  Every Mac older than that can no longer start this binary, and "
            "nothing else in the build would have told you.",
            file=sys.stderr,
        )
        print(
            "  Set MACOSX_DEPLOYMENT_TARGET for the lane rather than raising "
            "the ceiling, unless the bump is intended.",
            file=sys.stderr,
        )

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
