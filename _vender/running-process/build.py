#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

if __name__ == "__main__":
    from ci.build_wheel import main

    raise SystemExit(main(default_mode="dev"))
