from __future__ import annotations

from pathlib import Path

from ci import check_rust_debug_annotations
from ci.tiny_pdb_symbols import TinyPdbSymbolSpec

PADDING = ["", "", "", "", "", ""]


def test_windows_boundary_files_have_required_tiny_pdb_annotations() -> None:
    assert check_rust_debug_annotations.missing_boundary_annotations() == []


def test_missing_boundary_annotations_reports_missing_wrapper_requirements(
    tmp_path: Path, monkeypatch
) -> None:
    path = tmp_path / "boundary.rs"
    lines = [
        "#[unsafe(no_mangle)]",
        "#[inline(never)]",
        'pub extern \"C\" fn ok() {}',
        *PADDING,
        "#[inline(never)]",
        'pub extern \"C\" fn missing_no_mangle() {}',
        *PADDING,
        "#[unsafe(no_mangle)]",
        'pub extern \"C\" fn missing_inline() {}',
        *PADDING,
        "#[unsafe(no_mangle)]",
        "#[inline(never)]",
        "pub fn missing_extern() {}",
    ]
    path.write_text("\n".join(lines), encoding="utf-8")

    monkeypatch.setattr(
        check_rust_debug_annotations,
        "TINY_PDB_SYMBOLS",
        (
            TinyPdbSymbolSpec(
                name="missing_no_mangle",
                source=path.name,
                needle='pub extern \"C\" fn missing_no_mangle() {}',
                category="api",
            ),
            TinyPdbSymbolSpec(
                name="missing_inline",
                source=path.name,
                needle='pub extern \"C\" fn missing_inline() {}',
                category="api",
            ),
            TinyPdbSymbolSpec(
                name="missing_extern",
                source=path.name,
                needle="pub fn missing_extern() {}",
                category="api",
            ),
        ),
    )

    assert check_rust_debug_annotations.missing_boundary_annotations(tmp_path) == [
        "missing_no_mangle: missing #[unsafe(no_mangle)]",
        "missing_inline: missing #[inline(never)]",
        'missing_extern: missing extern "C"',
    ]
