#!/usr/bin/env python3
"""PostToolUse hook: runs per-file lint on edited Rust files.

Delegates to ./lint in single-file mode for speed.
Runs after every Edit/Write on .rs files.

Exit codes:
  0 - Success or non-Rust file
  2 - Lint violations found (stderr fed back to Claude)
"""

import json
import os
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_ROOT = SCRIPT_DIR.parent.parent


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0

    file_path = data.get("tool_input", {}).get("file_path", "")
    if not file_path:
        return 0

    # Normalize path
    file_path = file_path.replace("\\", "/")

    # Resolve relative paths against project root
    if not os.path.isabs(file_path):
        file_path = os.path.join(str(PROJECT_ROOT), file_path).replace("\\", "/")

    # Only lint Rust files
    if not file_path.endswith(".rs"):
        return 0

    # Skip deleted files
    if not os.path.isfile(file_path):
        return 0

    # Delegate to ci.lint in single-file mode
    result = subprocess.run(
        ["uv", "run", "python", "-m", "ci.lint", file_path],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        cwd=str(PROJECT_ROOT),
    )

    if result.returncode != 0:
        rel_path = os.path.relpath(file_path, str(PROJECT_ROOT))
        print(f"Lint violations in {rel_path}:", file=sys.stderr)
        if result.stdout.strip():
            print(result.stdout.strip(), file=sys.stderr)
        if result.stderr.strip():
            print(result.stderr.strip(), file=sys.stderr)
        return 2

    return 0


if __name__ == "__main__":
    sys.exit(main())
