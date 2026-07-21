from __future__ import annotations

from pathlib import Path


WORKFLOW = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "workflows"
    / "vendor-state.yml"
)


def test_vendor_state_workflow_has_read_only_contents_permission() -> None:
    """Keep the vendor-state check least-privileged as the workflow evolves."""
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()

    permissions_index = lines.index("permissions:")
    jobs_index = lines.index("jobs:")

    assert permissions_index < jobs_index
    assert lines[permissions_index + 1] == "  contents: read"

    # No broader workflow or job permission may be added without updating this
    # regression guard and consciously reviewing the required scope.
    permission_block = lines[permissions_index:jobs_index]
    assert all("write" not in line.lower() for line in permission_block)
