#!/usr/bin/env python3
"""Append the zccache release marker footer to built binaries."""

from __future__ import annotations

import argparse
from pathlib import Path

MARKER_MAGIC = b"ZCCSYMv1"
MARKER_SIZE = 128

OFFSET_SHA = 0
OFFSET_VERSION = 40
OFFSET_TRIPLE = 56
OFFSET_TIMESTAMP = 88
OFFSET_MAGIC = 120

SHA_LEN = 40
VERSION_LEN = 16
TRIPLE_LEN = 32


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--triple", required=True)
    parser.add_argument("--timestamp", type=int, required=True)
    return parser.parse_args()


def _write_nul_padded(out: bytearray, offset: int, size: int, value: str, field: str) -> None:
    encoded = value.encode("utf-8")
    if len(encoded) > size:
        raise ValueError(f"{field}: {len(encoded)} bytes exceeds slot size {size}")
    out[offset : offset + len(encoded)] = encoded


def encode_marker(*, git_sha: str, version: str, triple: str, build_timestamp: int) -> bytes:
    if build_timestamp < 0:
        raise ValueError("timestamp must be non-negative")

    out = bytearray(MARKER_SIZE)
    _write_nul_padded(out, OFFSET_SHA, SHA_LEN, git_sha, "git_sha")
    _write_nul_padded(out, OFFSET_VERSION, VERSION_LEN, version, "version")
    _write_nul_padded(out, OFFSET_TRIPLE, TRIPLE_LEN, triple, "triple")
    out[OFFSET_TIMESTAMP : OFFSET_TIMESTAMP + 8] = build_timestamp.to_bytes(8, "little")
    out[OFFSET_MAGIC : OFFSET_MAGIC + len(MARKER_MAGIC)] = MARKER_MAGIC
    return bytes(out)


def append_marker(binary: Path, marker: bytes) -> None:
    with binary.open("ab") as fh:
        fh.write(marker)
        fh.flush()


def main() -> None:
    args = parse_args()
    marker = encode_marker(
        git_sha=args.sha,
        version=args.version,
        triple=args.triple,
        build_timestamp=args.timestamp,
    )
    append_marker(args.binary, marker)
    print(f"stamped {args.binary} ({len(marker)} bytes appended)")


if __name__ == "__main__":
    main()
