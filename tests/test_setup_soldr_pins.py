from __future__ import annotations

from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
OLD_SETUP_SOLDR_SHA = "1937c19529f3690df5553a36dd33f39ccb20b070"
SETUP_SOLDR_V0_2_SHA = "13b2e37f3ee8dc6867f08d3b2fe49ece4783dba2"


def test_workflows_pin_setup_soldr_v0_2() -> None:
    workflow_paths = sorted((REPO_ROOT / ".github" / "workflows").glob("*.yml"))
    workflow_text = "\n".join(
        path.read_text(encoding="utf-8") for path in workflow_paths
    )

    assert OLD_SETUP_SOLDR_SHA not in workflow_text
    assert workflow_text.count(f"zackees/setup-soldr@{SETUP_SOLDR_V0_2_SHA}") == 5
    assert "v0.1.0 / v0" not in workflow_text
    assert "v0.2.0 / v0" in workflow_text
