"""Shared release-artifact naming policy tests."""

from __future__ import annotations

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).parents[1]
SCRIPTS = REPO_ROOT / ".github" / "scripts"

artifacts = load_script_module(SCRIPTS / "release_artifacts.py", "release_artifacts")


def test_windows_msvc_targets_have_the_executable_suffix() -> None:
    assert artifacts.binary_suffix("x86_64-pc-windows-msvc") == ".exe"
    assert artifacts.binary_suffix("aarch64-pc-windows-msvc") == ".exe"


def test_non_windows_targets_have_no_executable_suffix() -> None:
    for target in (
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
    ):
        assert artifacts.binary_suffix(target) == ""


def test_runner_os_controls_the_host_driver_suffix() -> None:
    assert artifacts.runner_binary_suffix("Windows") == ".exe"
    assert artifacts.runner_binary_suffix("Linux") == ""
    assert artifacts.runner_binary_suffix("macOS") == ""


def test_release_version_normalization_removes_only_the_prefix() -> None:
    assert artifacts.normalized_release_version("v0.9.2") == "0.9.2"
    assert artifacts.normalized_release_version("0.9.2") == "0.9.2"


def test_version_json_status_requires_a_parseable_exact_payload() -> None:
    assert (
        artifacts.version_json_status('{ "soldr_version" : "0.9.2" }', "0.9.2") is None
    )
    assert artifacts.version_json_status("", "0.9.2") == "empty"
    assert (
        artifacts.version_json_status('{"soldr_version":"0.0.1"}', "0.9.2")
        == "mismatch"
    )
    assert (
        artifacts.version_json_status('warning\n{"soldr_version":"0.9.2"}', "0.9.2")
        == "invalid"
    )


def test_release_scripts_share_the_same_suffix_policy() -> None:
    consumers = [
        "fetch_release_support_binaries",
        "release_archive_smoke",
        "release_manifest",
        "stage_release_binaries",
        "verify_release_bundle",
    ]
    for consumer in consumers:
        shared = load_script_module(
            SCRIPTS / "release_artifacts.py", "release_artifacts"
        )
        module = load_script_module(SCRIPTS / f"{consumer}.py", consumer)
        assert module.binary_suffix is shared.binary_suffix


def test_runner_scripts_share_the_same_host_suffix_policy() -> None:
    for consumer in ("package_release_archive", "prepare_release_wheel"):
        shared = load_script_module(
            SCRIPTS / "release_artifacts.py", "release_artifacts"
        )
        module = load_script_module(SCRIPTS / f"{consumer}.py", consumer)
        assert module.runner_binary_suffix is shared.runner_binary_suffix
