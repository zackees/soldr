from __future__ import annotations

import importlib.util
from pathlib import Path


SCRIPT = Path(__file__).with_name("windows_msvc_cache_roundtrip.py")
SPEC = importlib.util.spec_from_file_location("windows_msvc_cache_roundtrip", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
roundtrip = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(roundtrip)


def test_build_roundtrip_requires_missing_pe_to_be_restored(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    target = "x86_64-pc-windows-msvc"
    profile = "ci-nextest"
    artifact = tmp_path / "target" / target / profile / "soldr.exe"
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b"MZcold")
    commands: list[list[str]] = []

    def fake_clean(*, target: str, profile: str) -> None:
        artifact.unlink()

    def fake_run(command: list[str]) -> None:
        commands.append(command)
        assert "--no-cache" not in command
        artifact.write_bytes(b"MZwarm")

    monkeypatch.setattr(roundtrip, "clean_first_party", fake_clean)
    monkeypatch.setattr(roundtrip, "run", fake_run)

    roundtrip.build_roundtrip(target=target, profile=profile)

    assert artifact.read_bytes() == b"MZwarm"
    assert commands == [
        [
            "soldr",
            "build",
            "--profile",
            profile,
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ]
    ]


def test_archive_roundtrip_rebuilds_without_bypass_and_preserves_cli(
    tmp_path: Path, monkeypatch
) -> None:
    monkeypatch.chdir(tmp_path)
    target = "aarch64-pc-windows-msvc"
    profile = "ci-nextest"
    artifact = tmp_path / "target" / target / profile / "soldr.exe"
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b"MZvalidated-cli")
    archive = tmp_path / "dist" / "windows-tests.tar.zst"
    archive.parent.mkdir()
    archive.write_bytes(b"cold archive")
    commands: list[list[str]] = []

    def fake_clean(*, target: str, profile: str) -> None:
        artifact.unlink()

    def fake_run(command: list[str]) -> None:
        commands.append(command)
        assert "--no-cache" not in command
        archive.write_bytes(b"warm archive")

    monkeypatch.setattr(roundtrip, "clean_first_party", fake_clean)
    monkeypatch.setattr(roundtrip, "run", fake_run)
    monkeypatch.setattr(roundtrip, "archive_members", lambda path: ["test.exe"])

    roundtrip.archive_roundtrip(
        target=target, profile=profile, archive=archive.relative_to(tmp_path)
    )

    assert artifact.read_bytes() == b"MZvalidated-cli"
    assert len(commands) == 1
    assert commands[0][:4] == ["soldr", "cargo", "nextest", "archive"]
    assert "-E" not in commands[0]
    assert "--filter-expr" not in commands[0]
