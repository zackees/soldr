#!/usr/bin/env python3
"""Lint the watcher Python API only."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    watcher_python = root / "crates" / "zccache-watcher" / "python" / "zccache" / "watcher"
    checker = root / "ci" / "lint_python" / "keyboard_interrupt_checker.py"
    result = subprocess.run(
        [sys.executable, str(checker), str(watcher_python)],
        cwd=root,
    )
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
