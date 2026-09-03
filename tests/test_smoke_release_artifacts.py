"""Unit coverage for ci/smoke_release_artifacts.py (soldr#2294, soldr#3071).

The native release smokes (smoke_macos_x64, smoke_windows in
release-auto.yml) execute this script; the target-dependent decisions are
pure functions so they are pinned here instead of being discovered on a
release run.
"""

import zipfile
from pathlib import Path

import pytest
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
    assert "smoke_macos_x64:" in release
    assert "smoke_windows:" in release


# ---------- soldr#3071: host-side checks that need no target execution ----------


def _write_wheel(tmp_path: Path, *, version: str) -> Path:
    wheel = tmp_path / f"soldr-{version}-cp313-abi3-macosx_11_0_arm64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "soldr-0.dist-info/METADATA",
            f"Metadata-Version: 2.1\nName: soldr\nVersion: {version}\n",
        )
    return wheel


def test_wheel_version_reads_metadata_without_executing_anything(
    tmp_path: Path,
) -> None:
    wheel = _write_wheel(tmp_path, version="0.9.10")
    assert MODULE.wheel_version(wheel) == "0.9.10"


def test_wheel_version_requires_exactly_one_metadata_entry(tmp_path: Path) -> None:
    wheel = tmp_path / "empty.whl"
    with zipfile.ZipFile(wheel, "w"):
        pass
    with pytest.raises(RuntimeError, match="exactly one"):
        MODULE.wheel_version(wheel)


def test_check_macho_architecture_accepts_a_matching_cputype(
    tmp_path: Path,
) -> None:
    binary = tmp_path / "soldr"
    # MH_MAGIC_64 + CPU_TYPE_ARM64, both little-endian, padded to 16 bytes.
    binary.write_bytes(MODULE.MACHO_MAGIC_64 + b"\x0c\x00\x00\x01" + b"\x00" * 8)
    MODULE.check_macho_architecture(binary, "arm64")  # must not raise/exit


def test_check_macho_architecture_rejects_a_mismatched_cputype(
    tmp_path: Path,
) -> None:
    binary = tmp_path / "soldr"
    binary.write_bytes(MODULE.MACHO_MAGIC_64 + b"\x0c\x00\x00\x01" + b"\x00" * 8)
    with pytest.raises(SystemExit, match="expected Mach-O cputype for x86_64"):
        MODULE.check_macho_architecture(binary, "x86_64")


def test_check_macho_architecture_rejects_a_non_macho_file(tmp_path: Path) -> None:
    binary = tmp_path / "soldr.exe"
    binary.write_bytes(b"MZ\x90\x00" + b"\x00" * 12)
    with pytest.raises(SystemExit, match="not a 64-bit Mach-O binary"):
        MODULE.check_macho_architecture(binary, "x86_64")


# ---------- soldr#3071: --exec-prefix routes execution through the guest ----------


def test_build_argv_runs_directly_without_an_exec_prefix() -> None:
    assert MODULE.build_argv(None, [Path("/extracted/soldr"), "--version"]) == [
        "/extracted/soldr",
        "--version",
    ]


def test_build_argv_prepends_and_shlex_splits_the_exec_prefix() -> None:
    # A `Path` argument is reduced to its basename: the guest only ever has
    # what `--guest-sync-dest` synced in at its `--cwd`, never the host's
    # absolute extraction path.
    argv = MODULE.build_argv(
        "python3 ci/macos_x64_guest.py exec --cwd /Users/runner/work/ws --",
        [Path("/host/extracted/soldr"), "--version"],
    )
    assert argv == [
        "python3",
        "ci/macos_x64_guest.py",
        "exec",
        "--cwd",
        "/Users/runner/work/ws",
        "--",
        "soldr",
        "--version",
    ]


def test_build_argv_keeps_full_paths_without_an_exec_prefix() -> None:
    argv = MODULE.build_argv(None, [Path("/host/extracted/soldr"), "--version"])
    assert argv == ["/host/extracted/soldr", "--version"]


def test_run_passes_the_built_argv_to_subprocess(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, list[str]] = {}

    class _Result:  # pylint: disable=too-few-public-methods
        stdout = "soldr 0.9.10\n"

    def fake_run(argv: list[str], **_kwargs: object) -> _Result:
        captured["argv"] = argv
        return _Result()

    monkeypatch.setattr(MODULE.subprocess, "run", fake_run)
    result = MODULE.run(
        [Path("/extracted/soldr"), "--version"], exec_prefix="guest-runner --"
    )
    assert captured["argv"] == ["guest-runner", "--", "soldr", "--version"]
    assert result.stdout == "soldr 0.9.10\n"


def test_sync_extracted_into_guest_calls_the_guest_script(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    captured: dict[str, list[str]] = {}

    def fake_run(argv: list[str], **_kwargs: object) -> None:
        captured["argv"] = argv

    monkeypatch.setattr(MODULE.subprocess, "run", fake_run)
    MODULE.sync_extracted_into_guest(Path("extracted"), "/Users/runner/work/ws/dist")
    argv = captured["argv"]
    assert argv[1:4] == [str(MODULE.GUEST_SCRIPT), "sync-in", "--src"]
    assert argv[4] == "extracted"
    assert argv[-2:] == ["--dest", "/Users/runner/work/ws/dist"]
