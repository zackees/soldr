"""Tests for release-wheel preparation extracted from release-auto.yml."""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"

wheel = load_script_module(
    SCRIPTS / "prepare_release_wheel.py", "prepare_release_wheel"
)


def test_driver_path_tracks_runner_os_not_cross_target(tmp_path: Path) -> None:
    driver_dir = tmp_path / "release"

    assert wheel.driver_path("Windows", driver_dir) == driver_dir / "soldr.exe"
    assert wheel.driver_path("Linux", driver_dir) == driver_dir / "soldr"
    assert wheel.driver_path("macOS", driver_dir) == driver_dir / "soldr"


def test_clean_wheel_outputs_removes_only_wheel_build_products(tmp_path: Path) -> None:
    dist = tmp_path / "dist"
    target = tmp_path / "target"
    dist.mkdir()
    (dist / "stale.whl").write_bytes(b"wheel")
    (dist / "keep.txt").write_text("keep", encoding="utf-8")
    (target / "wheels").mkdir(parents=True)
    (target / "wheels" / "wheel.whl").write_bytes(b"wheel")
    (target / "maturin").mkdir()
    (target / "maturin" / "state").write_text("state", encoding="utf-8")
    nested = target / "other" / "nested.whl"
    nested.parent.mkdir(parents=True)
    nested.write_bytes(b"wheel")

    wheel.clean_wheel_outputs(tmp_path)

    assert not (dist / "stale.whl").exists()
    assert (dist / "keep.txt").read_text(encoding="utf-8") == "keep"
    assert not (target / "wheels").exists()
    assert not (target / "maturin").exists()
    assert not nested.exists()


def test_installed_package_version_reads_the_soldr_cli_metadata(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    metadata = {
        "packages": [
            {"name": "other", "version": "0"},
            {"name": "soldr-cli", "version": "0.9.2"},
        ]
    }

    def fake_run(command: list[str], **kwargs: object) -> SimpleNamespace:
        assert command[1:3] == ["cargo", "metadata"]
        assert kwargs["check"] is False
        return SimpleNamespace(returncode=0, stdout=json.dumps(metadata))

    monkeypatch.setattr(wheel.subprocess, "run", fake_run)

    assert wheel.installed_package_version(Path("soldr"), cwd=tmp_path) == "0.9.2"


@pytest.mark.parametrize(
    ("observed", "expected", "message"),
    [
        ("", "v0.9.2", "returned no version"),
        ("0.9.1", "v0.9.2", "does not match"),
    ],
)
def test_validate_workspace_version_rejects_missing_or_drifted_metadata(
    observed: str,
    expected: str,
    message: str,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        wheel, "installed_package_version", lambda *_args, **_kwargs: observed
    )

    with pytest.raises(wheel.WheelPreparationError, match=message):
        wheel.validate_workspace_version(Path("soldr"), expected, cwd=tmp_path)


def test_empty_setup_soldr_hook_uses_the_documented_default() -> None:
    assert wheel.resolved_hook("") == "python -m build --wheel"
    assert wheel.resolved_hook("  ") == "python -m build --wheel"
    assert wheel.resolved_hook("python -m build --wheel") == "python -m build --wheel"


def test_prepare_runs_cleanup_validation_and_builder(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[tuple[list[str], bool]] = []
    monkeypatch.setattr(wheel, "clean_wheel_outputs", lambda _root: None)
    monkeypatch.setattr(
        wheel, "validate_workspace_version", lambda *_args, **_kwargs: None
    )

    def fake_run(
        command: list[str], *, cwd: Path, check: bool, **_kwargs: object
    ) -> SimpleNamespace:
        calls.append((command, check))
        return SimpleNamespace(returncode=0, stdout="")

    monkeypatch.setattr(wheel.subprocess, "run", fake_run)

    wheel.prepare_and_build(
        target="x86_64-pc-windows-msvc",
        runner_os="Windows",
        expected_version="v0.9.2",
        wheel_hook="",
        repo_root=tmp_path,
        driver_dir=tmp_path / "release",
    )

    driver = str(tmp_path / "release" / "soldr.exe")
    assert calls[0] == (
        [
            driver,
            "cargo",
            "clean",
            "-p",
            "soldr-cli",
            "--target",
            "x86_64-pc-windows-msvc",
            "--release",
        ],
        False,
    )
    assert calls[1] == (
        [
            "git",
            "restore",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "crates/soldr-cli/Cargo.toml",
        ],
        False,
    )
    assert calls[2] == (["uv", "python", "install", "3.13"], True)
    assert calls[3] == (
        wheel.builder_command("x86_64-pc-windows-msvc", wheel.DEFAULT_WHEEL_HOOK),
        True,
    )


def test_workflow_invokes_preparation_script_instead_of_inline_wheel_policy() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert ".github/scripts/prepare_release_wheel.py" in workflow
    assert "cargo metadata returned no version for soldr-cli" not in workflow
    assert "setup-soldr wheel hook:" not in workflow
