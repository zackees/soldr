#!/usr/bin/env python3
"""PostToolUse hook: enforces README.md in every directory.

After any Edit/Write, checks that the directory containing the edited
file has a README.md. Feeds an error back to Claude if missing.

Exit codes:
  0 - README.md exists or check not applicable
  2 - README.md missing (stderr fed back to Claude)
"""

import json
import os
import sys
from pathlib import Path

EXCLUDED_DIRS = {".git", ".github", "target", ".loop", "__pycache__", ".venv", "node_modules"}


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
    filename = os.path.basename(file_path)
    directory = os.path.dirname(file_path)

    # If the file being written IS a README.md, no need to check
    if filename == "README.md":
        return 0

    # Skip excluded directories
    parts = Path(file_path).parts
    if any(part in EXCLUDED_DIRS for part in parts):
        return 0

    # Check for README.md
    if os.path.isfile(os.path.join(directory, "README.md")):
        return 0

    # Try original (un-normalized) path too
    orig_path = data.get("tool_input", {}).get("file_path", "")
    orig_dir = os.path.dirname(orig_path)
    if orig_dir and os.path.isfile(os.path.join(orig_dir, "README.md")):
        return 0

    print(f"Missing README.md in directory: {directory}", file=sys.stderr)
    print("Every directory must have a README.md. Please create one.", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
