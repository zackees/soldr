from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


def _binary_name() -> str:
    return "zccache.exe" if sys.platform == "win32" else "zccache"


def find_binary() -> Path:
    current = Path(sys.executable).resolve().parent
    candidate = current / _binary_name()
    if candidate.exists():
        return candidate
    for entry in map(Path, os.environ.get("PATH", "").split(os.pathsep)):
        if not entry:
            continue
        candidate = entry / _binary_name()
        if candidate.exists():
            return candidate
    raise FileNotFoundError("cannot find zccache binary on PATH")


def main(argv: list[str] | None = None) -> int:
    args = list(argv or [])
    result = subprocess.run([str(find_binary()), *args], check=False)
    return int(result.returncode)
