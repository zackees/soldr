"""Unit coverage for ci/smoke_release_artifacts.py (soldr#2294, soldr#3076).

The native release smokes (smoke_macos_x64, smoke_windows in
release-auto.yml) execute this script; the target-dependent decisions are
pure functions so they are pinned here instead of being discovered on a
release run.
"""

import zipfile
from pathlib import Path

import pytest
from conftest import assert_recovery_verify_collected_contract, load_script_module

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


# ---------- host-side checks that need no target execution ----------


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


def test_run_passes_the_argv_to_subprocess(monkeypatch: pytest.MonkeyPatch) -> None:
    captured: dict[str, list[str]] = {}

    class _Result:  # pylint: disable=too-few-public-methods
        stdout = "soldr 0.9.10\n"

    def fake_run(argv: list[str], **_kwargs: object) -> _Result:
        captured["argv"] = argv
        return _Result()

    monkeypatch.setattr(MODULE.subprocess, "run", fake_run)
    result = MODULE.run([Path("/extracted/soldr"), "--version"])
    assert captured["argv"] == ["/extracted/soldr", "--version"]
    assert result.stdout == "soldr 0.9.10\n"


# ---------- soldr#3076: Recovery guest-script generation ----------


def test_copy_into_share_dir_stages_every_guest_binary(tmp_path: Path) -> None:
    extract = tmp_path / "extracted"
    extract.mkdir()
    for name, _check in MODULE.GUEST_BINARY_CHECKS:
        (extract / name).write_bytes(b"fake binary")
    share = tmp_path / "share"
    MODULE.copy_into_share_dir(extract, share, "")
    for name, _check in MODULE.GUEST_BINARY_CHECKS:
        assert (share / name).read_bytes() == b"fake binary"


def test_copy_into_share_dir_fails_when_a_binary_is_missing(tmp_path: Path) -> None:
    extract = tmp_path / "extracted"
    extract.mkdir()
    share = tmp_path / "share"
    with pytest.raises(SystemExit, match="missing"):
        MODULE.copy_into_share_dir(extract, share, "")


def test_build_release_guest_script_fetches_every_binary_by_basename() -> None:
    script = MODULE.build_release_guest_script("0.9.11")
    assert script.startswith("#!/bin/sh")
    for name, _check in MODULE.GUEST_BINARY_CHECKS:
        assert f"curl -fsS -o /tmp/{name} {MODULE.GUEST_HTTP_BASE}/{name}" in script
    assert 'exit "$FAIL"' in script


def test_build_release_guest_script_checks_the_expected_version() -> None:
    script = MODULE.build_release_guest_script("0.9.11")
    assert '"soldr_version": "0.9.11"' in script


def test_build_release_guest_script_help_check_for_daemon() -> None:
    script = MODULE.build_release_guest_script("0.9.11")
    assert "/tmp/soldr-daemon --help" in script
    assert 'echo "soldr-daemon_help=pass"' in script


def test_parse_summary_splits_status_and_detail() -> None:
    text = "arch=pass:x86_64\nversion=fail:not soldr\nempty=\n\n"
    results = MODULE.parse_summary(text)
    assert results["arch"] == (True, "x86_64")
    assert results["version"] == (False, "not soldr")
    assert results["empty"] == (False, "")


def test_parse_summary_flags_malformed_lines() -> None:
    results = MODULE.parse_summary("not a key value line\n")
    assert results["summary_line_1"] == (
        False,
        "malformed line: 'not a key value line'",
    )


def _passing_summary_lines() -> list[str]:
    lines = ["arch=pass:x86_64"]
    for name, check in MODULE.GUEST_BINARY_CHECKS:
        lines.append(f"fetch_{name}=pass")
        lines.append(f"{name}_{check}=pass:ok")
    lines.append('soldr_version_json=pass:{"soldr_version": "0.9.11"}')
    return lines


def test_verify_collected_matches_the_shared_recovery_contract(tmp_path: Path) -> None:
    assert_recovery_verify_collected_contract(
        MODULE, tmp_path, passing_lines=_passing_summary_lines()
    )
