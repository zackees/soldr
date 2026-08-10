from __future__ import annotations

import json
import zipfile
from pathlib import Path

import pytest

from ci import verify_release_symbols
from ci.tiny_pdb_symbols import TINY_PDB_SYMBOLS
from ci.wheel_record import render_record


def _write_release_wheel(tmp_path: Path, *, pdb_bytes: bytes = b"x" * 733_184) -> Path:
    wheel = tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl"
    manifest = {
        "schema_version": 1,
        "symbols": [spec.__dict__ for spec in TINY_PDB_SYMBOLS],
    }
    files = [
        ("running_process/_native.cp313-win_amd64.pyd", b"pyd-bytes"),
        ("running_process/_native.cp313-win_amd64.pdb", pdb_bytes),
        (
            "running_process/_native.cp313-win_amd64.tiny-pdb.json",
            json.dumps(manifest).encode("utf-8"),
        ),
    ]
    record_entry = "running_process-3.0.3.dist-info/RECORD"
    with zipfile.ZipFile(wheel, "w") as zf:
        for name, payload in files:
            zf.writestr(name, payload)
        zf.writestr(record_entry, render_record(files, record_entry))
    return wheel


def _fake_publics_text(*symbols: str) -> str:
    return "\n".join(
        f"pub {index}: `{symbol}`" for index, symbol in enumerate(symbols, start=1)
    )


def test_wheel_native_entries_reads_packaged_artifacts(tmp_path: Path) -> None:
    wheel = _write_release_wheel(tmp_path)

    entries = verify_release_symbols.wheel_native_entries(wheel)

    assert entries["pyd_entry"] == "running_process/_native.cp313-win_amd64.pyd"
    assert entries["pdb_entry"] == "running_process/_native.cp313-win_amd64.pdb"
    assert (
        entries["manifest_entry"]
        == "running_process/_native.cp313-win_amd64.tiny-pdb.json"
    )
    assert entries["pdb_size"] == 733_184


def test_verify_release_artifact_checks_summary_symbols_and_sizes(
    tmp_path: Path, monkeypatch
) -> None:
    wheel = _write_release_wheel(tmp_path)

    monkeypatch.setattr(
        verify_release_symbols,
        "_resolve_llvm_pdbutil",
        lambda explicit=None: "llvm-pdbutil",
    )

    def fake_run(pdbutil: str, args: list[str], *, pdb_path: Path) -> str:
        del pdbutil, pdb_path
        if args == ["dump", "-summary"]:
            return "\n".join(
                [
                    "Has Publics: true",
                    "Is stripped: true",
                ]
            )
        if args == ["dump", "-publics"]:
            return _fake_publics_text(*[spec.name for spec in TINY_PDB_SYMBOLS])
        raise AssertionError(args)

    monkeypatch.setattr(verify_release_symbols, "_run_pdbutil", fake_run)

    report = verify_release_symbols.verify_release_artifact(wheel)

    assert report["pdb_size"] == 733_184
    assert report["public_symbol_count"] == len(TINY_PDB_SYMBOLS)
    assert report["ideal_size_met"] is False


def test_verify_release_artifact_fails_low_water(tmp_path: Path, monkeypatch) -> None:
    wheel = _write_release_wheel(tmp_path, pdb_bytes=b"x" * 64)
    monkeypatch.setattr(
        verify_release_symbols,
        "_resolve_llvm_pdbutil",
        lambda explicit=None: "llvm-pdbutil",
    )
    monkeypatch.setattr(
        verify_release_symbols, "_run_pdbutil", lambda *args, **kwargs: ""
    )

    with pytest.raises(RuntimeError, match="too small"):
        verify_release_symbols.verify_release_artifact(wheel)


def test_verify_release_artifact_fails_high_water_before_pdbutil(
    tmp_path: Path, monkeypatch
) -> None:
    wheel = _write_release_wheel(
        tmp_path,
        pdb_bytes=b"x" * (verify_release_symbols.DEFAULT_PDB_HIGH_WATER_BYTES + 1),
    )
    calls = 0

    def fake_resolve(explicit=None):
        nonlocal calls
        calls += 1
        return "llvm-pdbutil"

    monkeypatch.setattr(verify_release_symbols, "_resolve_llvm_pdbutil", fake_resolve)

    with pytest.raises(RuntimeError, match="too large"):
        verify_release_symbols.verify_release_artifact(wheel)
    assert calls == 0


def test_verify_release_artifact_fails_when_public_symbol_is_missing(
    tmp_path: Path, monkeypatch
) -> None:
    wheel = _write_release_wheel(tmp_path)
    monkeypatch.setattr(
        verify_release_symbols,
        "_resolve_llvm_pdbutil",
        lambda explicit=None: "llvm-pdbutil",
    )

    def fake_run(pdbutil: str, args: list[str], *, pdb_path: Path) -> str:
        del pdbutil, pdb_path
        if args == ["dump", "-summary"]:
            return "\n".join(
                [
                    "Has Publics: true",
                    "Is stripped: true",
                ]
            )
        if args == ["dump", "-publics"]:
            return _fake_publics_text(TINY_PDB_SYMBOLS[0].name)
        raise AssertionError(args)

    monkeypatch.setattr(verify_release_symbols, "_run_pdbutil", fake_run)

    with pytest.raises(RuntimeError, match="does not match allowlist exactly"):
        verify_release_symbols.verify_release_artifact(wheel)


def test_verify_release_artifact_rejects_disallowed_symbol_families(
    tmp_path: Path, monkeypatch
) -> None:
    wheel = _write_release_wheel(tmp_path)
    monkeypatch.setattr(
        verify_release_symbols,
        "_resolve_llvm_pdbutil",
        lambda explicit=None: "llvm-pdbutil",
    )

    def fake_run(pdbutil: str, args: list[str], *, pdb_path: Path) -> str:
        del pdbutil, pdb_path
        if args == ["dump", "-summary"]:
            return "\n".join(
                [
                    "Has Publics: true",
                    "Is stripped: true",
                ]
            )
        if args == ["dump", "-publics"]:
            symbols = [spec.name for spec in TINY_PDB_SYMBOLS]
            symbols[-1] = "pyo3_leak_symbol"
            return _fake_publics_text(*symbols)
        raise AssertionError(args)

    monkeypatch.setattr(verify_release_symbols, "_run_pdbutil", fake_run)

    with pytest.raises(RuntimeError, match="does not match allowlist exactly"):
        verify_release_symbols.verify_release_artifact(wheel)


def test_verify_release_artifact_rejects_stale_wheel_record(tmp_path: Path) -> None:
    wheel = tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl"
    manifest = {
        "schema_version": 1,
        "symbols": [spec.__dict__ for spec in TINY_PDB_SYMBOLS],
    }
    record_entry = "running_process-3.0.3.dist-info/RECORD"
    with zipfile.ZipFile(wheel, "w") as zf:
        zf.writestr("running_process/_native.cp313-win_amd64.pyd", b"pyd-bytes")
        zf.writestr("running_process/_native.cp313-win_amd64.pdb", b"x" * 733_184)
        zf.writestr(
            "running_process/_native.cp313-win_amd64.tiny-pdb.json",
            json.dumps(manifest),
        )
        zf.writestr(
            record_entry,
            "\n".join(
                [
                    "running_process/_native.cp313-win_amd64.pyd,sha256=stale,9",
                    "running_process/_native.cp313-win_amd64.pdb,sha256=stale,733184",
                    (
                        "running_process/_native.cp313-win_amd64.tiny-pdb.json,"
                        "sha256=stale,2"
                    ),
                    f"{record_entry},,",
                ]
            ),
        )

    with pytest.raises(RuntimeError, match="wheel RECORD does not match file contents"):
        verify_release_symbols.verify_release_artifact(wheel)


def test_verify_release_artifact_requires_packaged_pdb(tmp_path: Path) -> None:
    wheel = tmp_path / "running_process-3.0.3-cp313-cp313-win_amd64.whl"
    with zipfile.ZipFile(wheel, "w") as zf:
        zf.writestr("running_process/_native.cp313-win_amd64.pyd", b"pyd-bytes")

    with pytest.raises(
        RuntimeError,
        match=(r"expected running_process/_native\.cp313-win_amd64\.pdb"),
    ):
        verify_release_symbols.wheel_native_entries(wheel)


def test_format_release_artifact_report_mentions_public_symbol_count() -> None:
    report = {
        "wheel": "C:/repo/dist/running_process-3.0.3-cp313-cp313-win_amd64.whl",
        "pyd_entry": "running_process/_native.cp313-win_amd64.pyd",
        "pyd_size": 2684416,
        "pdb_entry": "running_process/_native.cp313-win_amd64.pdb",
        "pdb_size": 733184,
        "combined_native_size": 3417600,
        "public_symbol_count": len(TINY_PDB_SYMBOLS),
        "ideal_size_met": False,
    }

    rendered = verify_release_symbols.format_release_artifact_report(report)

    assert "public_symbols=" in rendered
    assert "ideal-size=no" in rendered
