"""Unit coverage for ci/smoke_release_artifacts.py (soldr#2294).

The native release smokes (smoke_macos_arm64, smoke_windows in
release-auto.yml) execute this script on scarce native runners; the
target-dependent decisions are pure functions so they are pinned here
instead of being discovered on a release run.
"""

from pathlib import Path

from conftest import load_script_module

REPO_ROOT = Path(__file__).resolve().parents[1]
MODULE = load_script_module(
    REPO_ROOT / "ci" / "smoke_release_artifacts.py", "smoke_release_artifacts"
)


def test_windows_targets_use_exe_suffix_everywhere() -> None:
    for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
        assert MODULE.exe_suffix(target) == ".exe"
        members = MODULE.required_members(target)
        assert "soldr.exe" in members
        assert "soldr-daemon.exe" in members
        assert "crgx.exe" in members
        assert "cargo-chef.exe" in members
        assert "manifest.json" in members


def test_unix_targets_have_no_suffix() -> None:
    for target in ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu"):
        assert MODULE.exe_suffix(target) == ""
        assert MODULE.required_members(target)[0] == "soldr"


def test_macho_arch_only_for_darwin() -> None:
    assert MODULE.macho_arch("aarch64-apple-darwin") == "arm64"
    assert MODULE.macho_arch("x86_64-apple-darwin") == "x86_64"
    assert MODULE.macho_arch("x86_64-pc-windows-msvc") is None
    assert MODULE.macho_arch("aarch64-unknown-linux-gnu") is None


def test_stub_floor_matches_the_workflow_contract() -> None:
    # 2 MiB, the soldr#1140 / soldr#1202 stub-binary floor used by the
    # inline archive smoke in release-auto.yml.
    assert MODULE.MIN_SOLDR_BYTES == 2 * 1024 * 1024


def test_workflow_smoke_jobs_invoke_the_script() -> None:
    release = (REPO_ROOT / ".github" / "workflows" / "release-auto.yml").read_text(
        encoding="utf-8"
    )
    assert release.count("ci/smoke_release_artifacts.py") >= 2
    assert "smoke_macos_arm64:" in release
    assert "smoke_windows:" in release
