"""Keep wheel cache diagnostics aligned with rust-cache's saved profile shape."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / ".github/scripts/cache_path_inventory.sh"
WORKFLOW = SCRIPT.parents[1] / "workflows/ci.yml"


def test_release_profiles_are_split_into_retained_components(tmp_path: Path) -> None:
    host = tmp_path / "target/release"
    cross = tmp_path / "target/aarch64-unknown-linux-gnu/release"
    for profile in (host, cross):
        for component in ("build", ".fingerprint", "deps"):
            directory = profile / component
            directory.mkdir(parents=True)
            (directory / "artifact").write_bytes(b"x" * 13)
    (cross / "deps" / "large.rlib").write_bytes(b"x" * 100)

    result = subprocess.run(
        ["bash", str(SCRIPT), "aarch64-unknown-linux-gnu"],
        cwd=tmp_path,
        env={**os.environ, "CARGO_HOME": str(tmp_path / "empty-cargo")},
        text=True,
        capture_output=True,
        check=True,
    )
    output = result.stdout
    for profile in (host, cross):
        relative = profile.relative_to(tmp_path)
        for component in ("build", ".fingerprint", "deps"):
            assert f"{relative}/{component}" in output
    assert "100\ttarget/aarch64-unknown-linux-gnu/release/deps/large.rlib" in output


def test_inventory_runs_after_wheel_checks_before_post_job_cache() -> None:
    workflow = WORKFLOW.read_text()
    wheel = workflow.index("- name: Build the wheel through `soldr wheel`")
    inventory = workflow.index("- name: Inventory wheel rust-cache inputs")
    assert wheel < inventory
    assert 'cache_path_inventory.sh "${{ matrix.target }}"' in workflow[inventory:]
