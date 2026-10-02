"""The quick gate's bootstrap soldr must embed the workspace's zccache (soldr#3458).

`_build-and-test.yml` builds the ci-test driver with setup-soldr's pinned
release against the Tier-2 zccache object store that the source-built soldr
saves. zccache keeps one store tree per version, so a bootstrap embedding a
different zccache gets zero hits: 0.9.16 (zccache 1.13.22) against a 1.14.14
store compiled all 381 units on every run. Nothing fails when that happens --
the step is just 139 s instead of a warm build -- so this guard makes the
drift loud: bumping Cargo.toml's zccache pin fails here until the bootstrap
pin (and its declared zccache) is moved to a release that embeds it.

The one exception is the bump itself: no soldr release can embed a zccache
before the PR that ingests it has shipped. That PR adds
`# bootstrap-zccache-pending: <new zccache>` beside the declaration; the
follow-up that moves the bootstrap to the first release embedding it removes
the marker. A marker that names anything but the workspace pin, or that
outlives the lag, fails.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml"
# soldr-daemon hosts the embedded zccache service whose store tree is versioned.
DAEMON_MANIFEST = "crates/soldr-daemon/Cargo.toml"
DECLARATION = re.compile(r"^\s*# bootstrap-embeds-zccache: (\S+)$", re.MULTILINE)
PENDING = re.compile(r"^\s*# bootstrap-zccache-pending: (\S+)$", re.MULTILINE)
SETUP_VERSION = re.compile(
    r"- name: Setup pinned soldr toolchain\n.*?\n\s+version: (\S+)\n", re.DOTALL
)
ZCCACHE_PIN = re.compile(r'^zccache = \{[^}]*version = "=([^"]+)"', re.MULTILINE)


def workspace_zccache(cargo_toml: str) -> str:
    match = ZCCACHE_PIN.search(cargo_toml)
    assert match, f"{DAEMON_MANIFEST} has no exact zccache pin"
    return match.group(1)


def declared_bootstrap_zccache(workflow: str) -> str:
    match = DECLARATION.search(workflow)
    assert match, "_build-and-test.yml lost its bootstrap-embeds-zccache declaration"
    return match.group(1)


def pending_bootstrap_zccache(workflow: str) -> str | None:
    match = PENDING.search(workflow)
    return match.group(1) if match else None


def bootstrap_matches_workspace(
    declared: str, pending: str | None, workspace: str
) -> bool:
    """True when the bootstrap embeds the workspace zccache, or the lag is declared.

    A pending marker is only valid while the declaration still lags: it must
    name the workspace pin and differ from the declared bootstrap zccache.
    """
    if pending is None:
        return declared == workspace
    return pending == workspace and declared != workspace


def bootstrap_version(workflow: str) -> str:
    match = SETUP_VERSION.search(workflow)
    assert match, "_build-and-test.yml has no pinned setup-soldr version"
    return match.group(1)


def test_bootstrap_declares_the_workspace_zccache() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    cargo = (REPO_ROOT / DAEMON_MANIFEST).read_text(encoding="utf-8")
    declared = declared_bootstrap_zccache(workflow)
    pending = pending_bootstrap_zccache(workflow)
    assert bootstrap_matches_workspace(declared, pending, workspace_zccache(cargo)), (
        "soldr-daemon's zccache pin moved: bump _build-and-test.yml's setup-soldr "
        "`version:` to a release embedding it, then update the declaration "
        "(a bump PR declares the lag with `# bootstrap-zccache-pending: <zccache>`; "
        "drop that marker once the bootstrap embeds it)"
    )


def test_declaration_matches_the_pinned_release_when_its_tag_is_present() -> None:
    """Cross-check the declaration against the release's own Cargo.toml.

    Needs the release tag locally (a full clone); the declaration test above
    is the one that runs everywhere.
    """
    workflow = WORKFLOW.read_text(encoding="utf-8")
    tag = f"v{bootstrap_version(workflow)}"
    shown = subprocess.run(
        ["git", "show", f"{tag}:{DAEMON_MANIFEST}"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if shown.returncode != 0:
        pytest.skip(f"release tag {tag} is not available in this checkout")
    assert workspace_zccache(shown.stdout) == declared_bootstrap_zccache(workflow)


def test_the_retired_pin_is_detected() -> None:
    """RED fixture: 0.9.16's zccache (1.13.22) is not the workspace's."""
    cargo = (REPO_ROOT / DAEMON_MANIFEST).read_text(encoding="utf-8")
    assert workspace_zccache(cargo) != "1.13.22"


def test_pending_marker_is_only_valid_while_the_bootstrap_lags() -> None:
    assert bootstrap_matches_workspace("1.14.14", None, "1.14.14")
    assert not bootstrap_matches_workspace("1.14.14", None, "1.15.0")
    assert bootstrap_matches_workspace("1.14.14", "1.15.0", "1.15.0")
    # The marker must name the workspace pin ...
    assert not bootstrap_matches_workspace("1.14.14", "1.14.99", "1.15.0")
    # ... and must be removed once the bootstrap catches up.
    assert not bootstrap_matches_workspace("1.15.0", "1.15.0", "1.15.0")
