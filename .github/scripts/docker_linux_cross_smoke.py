#!/usr/bin/env python3
"""Exercise the Windows-host Docker Linux cross-build surface (soldr#2319)."""

from __future__ import annotations

import argparse
import os
import struct
import subprocess
import sys
from pathlib import Path


def is_elf_amd64(path: Path) -> tuple[bool, str]:
    try:
        data = path.read_bytes()[:32]
    except OSError as exc:
        return False, f"cannot read {path}: {exc}"
    if len(data) < 20 or data[:4] != b"\x7fELF":
        return False, "missing ELF signature"
    if data[4:6] != b"\x02\x01":
        return False, "not a 64-bit little-endian ELF"
    if struct.unpack_from("<H", data, 18)[0] != 62:
        return False, "ELF machine is not x86-64"
    return True, "ELF64 x86-64"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--soldr", required=True)
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()

    fixture = args.fixture.resolve()
    command = [args.soldr, "--no-cache", "build", "--target", args.target]
    print(f"$ {' '.join(command)}  (cwd={fixture})", flush=True)
    result = subprocess.run(command, cwd=fixture, check=False)
    if result.returncode:
        return result.returncode or 1

    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", fixture / "target"))
    binary = target_dir / args.target / "debug" / "docker-cross-c"
    ok, reason = is_elf_amd64(binary)
    report = (
        "## Windows-host Docker Linux cross smoke\n\n"
        f"- target: `{args.target}`\n"
        f"- fixture: `{fixture.name}` (C + C++)\n"
        f"- output: `{binary}`\n"
        f"- verdict: {'OK' if ok else 'FAIL'} — {reason}\n"
    )
    print(report)
    if args.summary:
        args.summary.open("a", encoding="utf-8").write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
