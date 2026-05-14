from __future__ import annotations

import importlib.util
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
VERIFY_SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_setup_soldr_pin.py"


def load_verify_module():
    spec = importlib.util.spec_from_file_location(
        "verify_setup_soldr_pin", VERIFY_SCRIPT_PATH
    )
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_workflows_pin_setup_soldr_to_current_v0_sha() -> None:
    module = load_verify_module()

    module.verify_setup_soldr_pins(REPO_ROOT)
