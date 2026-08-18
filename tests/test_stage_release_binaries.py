"""Tests for the extracted release binary-staging gate (soldr#2469)."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"

stage = load_script_module(
    SCRIPTS / "stage_release_binaries.py", "stage_release_binaries"
)


def write_file(directory: Path, name: str, contents: bytes = b"artifact") -> Path:
    path = directory / name
    path.write_bytes(contents)
    return path


def test_windows_requires_and_stages_a_pdb_sidecar(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr.exe", b"soldr")
    write_file(release, "soldr_cli.pdb", b"symbols")

    staged = stage.stage_release_binaries("x86_64-pc-windows-msvc", release, package)

    assert [path.name for path in staged] == [
        "soldr.exe",
        "soldr-daemon.exe",
        "soldr_cli.pdb",
    ]
    assert (package / "soldr.exe").read_bytes() == b"soldr"
    assert (package / "soldr-daemon.exe").read_bytes() == b"soldr"
    assert (package / "soldr_cli.pdb").read_bytes() == b"symbols"


def test_windows_missing_pdb_is_a_named_release_failure(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "soldr.exe")

    with pytest.raises(stage.StagingError, match="PDB sidecar"):
        stage.stage_release_binaries(
            "x86_64-pc-windows-msvc", release, tmp_path / "package"
        )


def test_linux_stages_optional_split_dwarf_when_present(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")
    write_file(release, "soldr_cli.dwp", b"debug")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-gnu", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon", "soldr_cli.dwp"]
    assert (package / "soldr_cli.dwp").read_bytes() == b"debug"


def test_linux_release_without_split_dwarf_still_stages_binary(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    release.mkdir()
    write_file(release, "soldr")

    staged = stage.stage_release_binaries("x86_64-unknown-linux-musl", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon"]


def test_macos_stages_optional_dsym_bundle(tmp_path: Path) -> None:
    release = tmp_path / "release"
    package = tmp_path / "package"
    dsym = release / "soldr.dSYM" / "Contents" / "Resources"
    dsym.mkdir(parents=True)
    write_file(release, "soldr")
    write_file(dsym, "DWARF", b"symbols")

    staged = stage.stage_release_binaries("aarch64-apple-darwin", release, package)

    assert [path.name for path in staged] == ["soldr", "soldr-daemon", "soldr.dSYM"]
    assert (
        package / "soldr.dSYM" / "Contents" / "Resources" / "DWARF"
    ).read_bytes() == b"symbols"


def test_missing_main_binary_reports_observed_release_directory(tmp_path: Path) -> None:
    release = tmp_path / "release"
    release.mkdir()
    write_file(release, "another-file")

    with pytest.raises(stage.StagingError, match="expected soldr") as error:
        stage.stage_release_binaries(
            "x86_64-unknown-linux-gnu", release, tmp_path / "package"
        )

    assert "another-file" in str(error.value)


def test_workflow_invokes_the_script_instead_of_inlining_binary_staging() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/stage_release_binaries.py" in workflow
    assert "pdb_src=" not in workflow
    assert "staged Linux split-DWARF sidecar" not in workflow
