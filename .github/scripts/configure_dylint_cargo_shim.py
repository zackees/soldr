#!/usr/bin/env python3
"""Create a Cargo shim that keeps Dylint's nested driver build on its nightly."""

from __future__ import annotations

import argparse
import os
import re
import stat
from pathlib import Path


VALID_TOOLCHAIN = re.compile(r"^[A-Za-z0-9_.-]+$")


def write_cargo_shim(output_dir: Path, toolchain: str, *, windows: bool) -> Path:
    """Write the platform Cargo shim and return its path."""
    if not VALID_TOOLCHAIN.fullmatch(toolchain):
        raise ValueError(f"invalid Rust toolchain name: {toolchain!r}")

    output_dir.mkdir(parents=True, exist_ok=True)
    if windows:
        path = output_dir / "cargo.cmd"
        path.write_text(
            "@echo off\r\n"
            f"set RUSTUP_TOOLCHAIN={toolchain}\r\n"
            f"soldr rustup run {toolchain} cargo %*\r\n"
            "exit /b %ERRORLEVEL%\r\n",
            encoding="utf-8",
        )
    else:
        path = output_dir / "cargo"
        path.write_text(
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            f'export RUSTUP_TOOLCHAIN="{toolchain}"\n'
            f'exec soldr rustup run "{toolchain}" cargo "$@"\n',
            encoding="utf-8",
        )
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--toolchain", required=True)
    args = parser.parse_args()
    write_cargo_shim(args.output_dir, args.toolchain, windows=os.name == "nt")
    print(args.output_dir.resolve())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
