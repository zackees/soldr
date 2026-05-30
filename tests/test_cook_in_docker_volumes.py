"""Regression-guard tests for ``bench/cook_in_docker.sh``.

The script bind-mounts the soldr repo at ``/work`` and runs ``cargo`` inside
the ``soldr-cook-dev`` image. By default that means cargo writes
``/work/target/`` and re-fetches the registry into a fresh CARGO_HOME on
every container start, both of which go through Windows + WSL2's 9P
translation layer that rewrites file mtimes per container start and
defeats cargo's mtime-based fingerprint check.

zccache filed the upstream issue (zccache #475 fix, soldr #593) and showed
the bind-mount → named-volume switch turning multi-minute no-op rebuilds
into ~1 s. This test pins the equivalent invariant for soldr's own
in-container build/test loop:

1. ``/work/target`` is overlaid by a named Docker volume so cargo's target/
   lives on Linux-native ext4 inside Docker's VFS.
2. ``CARGO_HOME`` is set to a path backed by a named Docker volume so the
   crates-io registry index is reused across container starts.

The tests parse ``bench/cook_in_docker.sh`` as a string — running the
script for real requires Docker plus several minutes of setup and is
out of scope for unit-level enforcement.
"""

from __future__ import annotations

import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "bench" / "cook_in_docker.sh"


@pytest.fixture(scope="module")
def script() -> str:
    assert SCRIPT_PATH.is_file(), f"missing {SCRIPT_PATH}"
    return SCRIPT_PATH.read_text(encoding="utf-8")


def _docker_run_block(script_text: str) -> str:
    """Return the contiguous slice of the script that contains the
    final ``docker run`` invocation. Used to assert that the named
    volumes are wired into the actual run, not just defined in a
    comment elsewhere."""
    m = re.search(r"docker run.*?(?=\Z|\n\n)", script_text, re.DOTALL)
    assert m, "could not locate the `docker run` block in cook_in_docker.sh"
    return m.group(0)


# Mount-source token — either a literal volume name (`cook-soldr-target`)
# or a bash variable reference (`$TARGET_VOLUME` / `${TARGET_VOLUME}`).
# Forbids leading `/`, `.`, `~`, `$REPO_ROOT`, `$(pwd)` and other path
# expressions that would indicate a host bind mount.
_MOUNT_SOURCE = r"""(?:"?\$\{?[A-Za-z_]\w*\}?"?|"?[A-Za-z][\w\-]*"?)"""


def _resolve_variable(script_text: str, name: str) -> str | None:
    """Find ``NAME=...`` assignment in the script and return its value
    (stripped of quotes). Returns None if not assigned. Used to verify
    that a `$VAR_NAME` mount source resolves to a literal volume name
    rather than a host path."""
    m = re.search(rf'^\s*{re.escape(name)}=("?)([^"\n#]+)\1\s*$', script_text, re.MULTILINE)
    if not m:
        return None
    return m.group(2).strip()


def _mount_source_resolves_to_volume_name(source: str, script_text: str) -> bool:
    """True if the captured mount source is a named-volume reference
    (either literal or via a shell variable whose value is a named
    volume — not a host path)."""
    source = source.strip().strip('"').strip("'")
    if source.startswith("$"):
        var = source.lstrip("$").strip("{}")
        resolved = _resolve_variable(script_text, var)
        if resolved is None:
            return False
        source = resolved.strip().strip('"').strip("'")
    # A host path starts with one of these. A named volume is a bare
    # identifier (alnum + hyphen + underscore), no slashes or `~`.
    return not source.startswith(("/", ".", "~", "$"))


def test_target_dir_is_named_docker_volume(script: str) -> None:
    """The named volume must be mounted at ``/work/target`` inside the
    `docker run` invocation — overlaying the bind-mount default so
    cargo's target/ lives on Linux-native ext4 (fast) instead of the
    WSL2 9P-translated bind mount (slow)."""
    block = _docker_run_block(script)
    target_mount = re.search(
        rf'-v\s+{_MOUNT_SOURCE}:"?(?:/work/target|/target)"?', block
    )
    assert target_mount, (
        "no named-volume mount found for /work/target or /target in the "
        "docker run block of bench/cook_in_docker.sh — the bind-mounted "
        "repo's target/ defeats cargo's incremental on Windows hosts "
        "(soldr #593 / zccache #475)."
    )
    source = target_mount.group(0).split("-v", 1)[1].split(":", 1)[0]
    assert _mount_source_resolves_to_volume_name(source, script), (
        f"target/ mount source {source!r} is not a named-volume name — "
        "it looks like a host path, which puts cargo's target/ back on "
        "the slow bind-mount layer."
    )


def test_cargo_home_is_named_volume(script: str) -> None:
    """``CARGO_HOME`` must point at a path backed by a named Docker
    volume so the cargo registry index is persisted across container
    starts. Without this, every container re-fetches ~175 MiB.

    Acceptance: either an explicit ``-e CARGO_HOME=...`` env var with a
    matching ``-v <named-volume>:<path>`` mount, or the volume mounted
    at ``/root/.cargo`` (cargo's default home) — both forms ensure
    persistence."""
    block = _docker_run_block(script)

    cargo_home_envs = re.findall(r"-e\s+CARGO_HOME=([^\s'\"]+)", block)
    if cargo_home_envs:
        target_path = cargo_home_envs[0]
    else:
        # Default cargo home in the container (`rust:1.94.1-bookworm`
        # runs as root, so it's /root/.cargo unless overridden).
        target_path = "/root/.cargo"

    volume_mount = re.search(
        rf'-v\s+{_MOUNT_SOURCE}:"?{re.escape(target_path)}"?', block
    )
    assert volume_mount, (
        f"no named-volume mount found for CARGO_HOME target ({target_path}) "
        "in the docker run block of bench/cook_in_docker.sh — every "
        "container start re-fetches the crates.io registry index "
        "(~175 MiB) without it."
    )
    source = volume_mount.group(0).split("-v", 1)[1].split(":", 1)[0]
    assert _mount_source_resolves_to_volume_name(source, script), (
        f"CARGO_HOME mount source {source!r} is not a named-volume name — "
        "it looks like a host path, which puts the cargo registry back "
        "on the slow bind-mount layer."
    )


def test_target_and_cargo_volumes_are_not_wiped_each_run(script: str) -> None:
    """``docker volume rm`` is fine for the ``~/.soldr/`` reset (the
    cook-shared-cache feature deliberately tests from a clean
    daemon state), but the target/cargo-home volumes must persist —
    otherwise we lose the very speedup we're enforcing."""
    # The existing `~/.soldr/` reset uses `$VOLUME` (singular). Capture
    # whatever volume names are removed and assert they don't intersect
    # with the target/cargo-home volume names introduced by the fix.
    rm_volumes = re.findall(
        r"docker volume rm[^\n]*\b(\w[\w\-]*)\b", script
    )
    rm_names = set(rm_volumes)
    # Crude but effective: the soldr-home volume's variable is `VOLUME=...`,
    # so the `docker volume rm "$VOLUME"` line resolves to that name.
    # We forbid any explicit rm of names containing `target` or `cargo`.
    bad = {n for n in rm_names if "target" in n.lower() or "cargo" in n.lower()}
    assert not bad, (
        f"cook_in_docker.sh removes volumes that hold cargo build state: {bad}. "
        "These must persist across runs for the named-volume speedup to "
        "work — only the ~/.soldr/ daemon-state volume should be reset."
    )
