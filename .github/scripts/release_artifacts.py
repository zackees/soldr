"""Shared release-artifact naming rules.

The release workflow stages, verifies, and describes the same executable set in
separate scripts.  Keep target-specific filename rules here so a new target ABI
cannot make one release gate look for a different artifact than the others.
"""

from __future__ import annotations

import json


def binary_suffix(target: str) -> str:
    """Return the executable suffix used by a release target."""
    return ".exe" if target.endswith("-pc-windows-msvc") else ""


def runner_binary_suffix(runner_os: str) -> str:
    """Return the executable suffix required by a CI runner operating system."""
    return ".exe" if runner_os == "Windows" else ""


def normalized_release_version(version: str) -> str:
    """Normalize the v-prefixed release version used by release workflows."""
    return version.removeprefix("v")


def version_json_status(output: str, expected_version: str) -> str | None:
    """Classify the stable JSON version contract shared by release smoke gates."""
    if not output.strip():
        return "empty"
    try:
        payload = json.loads(output)
    except json.JSONDecodeError:
        return "invalid"
    if (
        not isinstance(payload, dict)
        or payload.get("soldr_version") != expected_version
    ):
        return "mismatch"
    return None
