"""Guard Soldr's split dependency-source contract.

zccache is compiled from the pinned `_vender/zccache` release submodule so its
embedded service is directly editable and auditable. running-process remains
an exact crates.io dependency so Cargo can share its artifacts across
checkouts. These assertions prevent either side from silently drifting back to
the other source model.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
ZCCACHE_PATH = "../../_vender/zccache/crates/zccache"


def workspace_manifests() -> list[Path]:
    manifests = [REPO_ROOT / "Cargo.toml"]
    manifests.extend(sorted((REPO_ROOT / "crates").glob("*/Cargo.toml")))
    return [path for path in manifests if path.is_file()]


def locked_packages() -> list[tuple[str, str | None]]:
    """Return `(name, source)` for lockfile packages; local paths lack source."""
    text = (REPO_ROOT / "Cargo.lock").read_text(encoding="utf-8")
    packages: list[tuple[str, str | None]] = []
    for block in text.split("[[package]]"):
        name = re.search(r'^name = "([^"]+)"', block, flags=re.MULTILINE)
        if not name:
            continue
        source = re.search(r'^source = "([^"]+)"', block, flags=re.MULTILINE)
        packages.append((name.group(1), source.group(1) if source else None))
    return packages


def dependency_lines(name: str) -> list[tuple[Path, str]]:
    pattern = re.compile(rf"^{re.escape(name)}\s*=\s*.+$", re.MULTILINE)
    found: list[tuple[Path, str]] = []
    for manifest in workspace_manifests():
        for match in pattern.finditer(manifest.read_text(encoding="utf-8")):
            found.append((manifest, match.group(0)))
    return found


def test_the_scan_reaches_manifests_and_lockfile() -> None:
    assert len(workspace_manifests()) >= 6
    packages = locked_packages()
    assert len(packages) > 100
    assert any(name == "zccache" for name, _ in packages)
    assert any(name == "running-process" for name, _ in packages)


def test_zccache_family_resolves_from_local_submodule() -> None:
    packages = [
        (name, source)
        for name, source in locked_packages()
        if name == "zccache" or name.startswith("zccache-")
    ]
    assert len(packages) >= 10, "vendored zccache internals disappeared from lockfile"
    nonlocal_packages = [(name, source) for name, source in packages if source]
    assert not nonlocal_packages, f"zccache must resolve locally: {nonlocal_packages}"


def test_zccache_manifests_use_released_path_dependency() -> None:
    specs = dependency_lines("zccache")
    assert len(specs) >= 3
    offenders = [
        f"{path.relative_to(REPO_ROOT)}: {line}"
        for path, line in specs
        if ZCCACHE_PATH not in line
        or 'version = "1.13.11"' not in line
        or "git" in line
    ]
    assert not offenders, "zccache source/version drift:\n" + "\n".join(offenders)


def test_running_process_stays_exact_and_registry_sourced() -> None:
    packages = [
        (name, source)
        for name, source in locked_packages()
        if name == "running-process" or name.startswith("running-process-")
    ]
    assert packages
    bad_packages = [
        (name, source)
        for name, source in packages
        if not source or not source.startswith("registry+")
    ]
    assert (
        not bad_packages
    ), f"running-process must stay registry sourced: {bad_packages}"

    specs = dependency_lines("running-process")
    assert len(specs) >= 3
    offenders = [
        f"{path.relative_to(REPO_ROOT)}: {line}"
        for path, line in specs
        if 'version = "=4.10.6"' not in line or "path" in line or "git" in line
    ]
    assert not offenders, "running-process source/version drift:\n" + "\n".join(
        offenders
    )


def test_gitmodules_declares_only_the_zccache_dependency_family() -> None:
    text = (REPO_ROOT / ".gitmodules").read_text(encoding="utf-8")
    assert '[submodule "_vender/zccache"]' in text
    assert "path = _vender/zccache" in text
    assert "url = https://github.com/zackees/zccache.git" in text
    assert "_vender/running-process" not in text


def test_root_excludes_only_the_nested_zccache_workspace() -> None:
    text = (REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
    assert '"_vender/zccache"' in text
    assert '"_vender/running-process"' not in text
