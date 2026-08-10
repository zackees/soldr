from __future__ import annotations

from collections import defaultdict
from pathlib import Path

from ci.tiny_pdb_symbols import ROOT, TINY_PDB_SYMBOLS, TinyPdbSymbolSpec


def _source_lines(root: Path, source: str) -> list[str]:
    return (root / source).read_text(encoding="utf-8").splitlines()


def _needle_line(lines: list[str], spec: TinyPdbSymbolSpec) -> int:
    for index, line in enumerate(lines, start=1):
        if spec.needle in line:
            return index
    raise RuntimeError(f"could not find {spec.needle!r} in {spec.source}")


def _window(lines: list[str], line_number: int) -> str:
    start = max(0, line_number - 5)
    end = min(len(lines), line_number + 3)
    return "\n".join(lines[start:end])


def missing_boundary_annotations(root: Path = ROOT) -> list[str]:
    grouped: dict[str, list[TinyPdbSymbolSpec]] = defaultdict(list)
    for spec in TINY_PDB_SYMBOLS:
        grouped[spec.source].append(spec)

    issues: list[str] = []
    for source, specs in grouped.items():
        lines = _source_lines(root, source)
        for spec in specs:
            line_number = _needle_line(lines, spec)
            snippet = _window(lines, line_number)
            if "#[unsafe(no_mangle)]" not in snippet:
                issues.append(f"{spec.name}: missing #[unsafe(no_mangle)]")
            if "#[inline(never)]" not in snippet:
                issues.append(f"{spec.name}: missing #[inline(never)]")
            if "extern \"C\"" not in snippet:
                issues.append(f"{spec.name}: missing extern \"C\"")
    return issues


def main() -> int:
    issues = missing_boundary_annotations()
    if not issues:
        return 0
    rendered = "\n".join(f"- {issue}" for issue in issues)
    raise SystemExit(f"missing tiny PDB wrapper annotations:\n{rendered}")


if __name__ == "__main__":
    raise SystemExit(main())
