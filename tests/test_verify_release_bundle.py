"""Tests for release-bundle portability dispatch (soldr#2469 step 2.2)."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-auto.yml"

bundle = load_script_module(
    SCRIPTS / "verify_release_bundle.py", "verify_release_bundle"
)


def write_bundle(package: Path, target: str) -> list[Path]:
    package.mkdir()
    suffix = bundle.binary_suffix(target)
    paths = [package / f"{stem}{suffix}" for stem in bundle.BINARY_STEMS]
    for path in paths:
        path.write_bytes(path.name.encode())
    return paths


def test_collects_every_windows_executable_in_bundle_order(tmp_path: Path) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "x86_64-pc-windows-msvc")

    assert bundle.bundled_binaries(package, "x86_64-pc-windows-msvc") == expected


def test_collects_every_unix_executable_in_bundle_order(tmp_path: Path) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "aarch64-unknown-linux-musl")

    assert bundle.bundled_binaries(package, "aarch64-unknown-linux-musl") == expected


def test_missing_bundle_fails_with_the_observed_package_contents(
    tmp_path: Path,
) -> None:
    package = tmp_path / "package"
    package.mkdir()
    (package / "manifest.json").write_text("{}", encoding="utf-8")

    with pytest.raises(
        bundle.BundleVerificationError, match="no bundled binaries"
    ) as error:
        bundle.bundled_binaries(package, "x86_64-unknown-linux-gnu")

    assert "manifest.json" in str(error.value)


@pytest.mark.parametrize(
    "case",
    [
        (
            "windows-imports",
            "x86_64-pc-windows-msvc",
            "2.39",
            "verify_windows_imports.py",
            [],
        ),
        (
            "macos-min-version",
            "aarch64-apple-darwin",
            "2.39",
            "verify_macos_min_version.py",
            ["--target", "aarch64-apple-darwin"],
        ),
        (
            "static",
            "x86_64-unknown-linux-musl",
            "2.39",
            "verify_static_link.py",
            [],
        ),
        (
            "glibc-baseline",
            "x86_64-unknown-linux-gnu",
            "2.39",
            "verify_glibc_baseline.py",
            ["--max-glibc", "2.39"],
        ),
    ],
)
def test_each_bundle_gate_delegates_to_its_established_verifier(
    case: tuple[str, str, str, str, list[str]], tmp_path: Path
) -> None:
    check, target, max_glibc, expected_script, expected_options = case
    binaries = write_bundle(tmp_path / "package", target)

    command = bundle.checker_command(check, target, binaries, max_glibc)

    assert command[:2] == [sys.executable, str(SCRIPTS / expected_script)]
    assert command[2 : 2 + len(expected_options)] == expected_options
    assert command[2 + len(expected_options) :] == [str(path) for path in binaries]


def test_verify_bundle_runs_verifier_with_all_staged_binaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    package = tmp_path / "package"
    expected = write_bundle(package, "x86_64-unknown-linux-musl")
    calls: list[tuple[list[str], bool]] = []

    def fake_run(command: list[str], check: bool) -> None:
        calls.append((command, check))

    monkeypatch.setattr(bundle.subprocess, "run", fake_run)

    bundle.verify_bundle("static", "x86_64-unknown-linux-musl", package, "2.39")

    assert calls == [
        (
            [
                sys.executable,
                str(SCRIPTS / "verify_static_link.py"),
                *(str(path) for path in expected),
            ],
            True,
        )
    ]


def test_workflow_invokes_dispatcher_instead_of_inline_bundle_collection() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert workflow.count(".github/scripts/verify_release_bundle.py") == 4
    assert "bundled=()" not in workflow
    assert "mapfile" not in workflow
