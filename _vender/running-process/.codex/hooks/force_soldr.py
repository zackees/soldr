from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from ci.codex_hooks import pre_tool_use_response


def main() -> int:
    payload = json.load(sys.stdin)
    response = pre_tool_use_response(payload)
    if response is not None:
        json.dump(response, sys.stdout)
        sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
