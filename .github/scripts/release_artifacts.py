"""Shared release-artifact naming rules.

The release workflow stages, verifies, and describes the same executable set in
separate scripts.  Keep target-specific filename rules here so a new target ABI
cannot make one release gate look for a different artifact than the others.
"""

from __future__ import annotations


def binary_suffix(target: str) -> str:
    """Return the executable suffix used by a release target."""
    return ".exe" if target.endswith("-pc-windows-msvc") else ""
