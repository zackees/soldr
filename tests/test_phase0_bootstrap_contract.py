from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
SETUP_SOLDR_0_9_74_SHA = "40320d277ba4946e38d4b3c02e6c7a15a29c3f3f"
DIRECT_USE = re.compile(r"uses:\s*zackees/setup-soldr@([0-9a-f]{40})")


def test_all_fourteen_setup_soldr_pins_consume_0_9_74() -> None:
    pins: list[tuple[str, str]] = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for match in DIRECT_USE.finditer(workflow.read_text(encoding="utf-8")):
            pins.append((workflow.name, match.group(1)))

    assert len(pins) == 14, pins
    assert {sha for _, sha in pins} == {SETUP_SOLDR_0_9_74_SHA}, pins
