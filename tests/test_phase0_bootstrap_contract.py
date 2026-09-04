from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
SETUP_SOLDR_V0_SHA = "5f1f68dcb8377818413c28ce52214261ae8ff771"
DIRECT_USE = re.compile(r"uses:\s*zackees/setup-soldr@([0-9a-f]{40})")


def test_all_fourteen_setup_soldr_pins_consume_current_v0() -> None:
    pins: list[tuple[str, str]] = []
    for workflow in sorted(WORKFLOWS.glob("*.y*ml")):
        for match in DIRECT_USE.finditer(workflow.read_text(encoding="utf-8")):
            pins.append((workflow.name, match.group(1)))

    assert len(pins) == 14, pins
    assert {sha for _, sha in pins} == {SETUP_SOLDR_V0_SHA}, pins
