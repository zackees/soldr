from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from zccache._native import convert_ino as _convert_ino


@dataclass(frozen=True)
class InoConvertResult:
    cache_hit: bool
    skipped_write: bool


def convert_ino(
    input: str | Path,
    output: str | Path,
    *,
    clang_args: list[str] | None = None,
    inject_arduino_include: bool = True,
) -> InoConvertResult:
    result = _convert_ino(
        str(Path(input)),
        str(Path(output)),
        list(clang_args or []),
        inject_arduino_include,
    )
    return InoConvertResult(
        cache_hit=result.cache_hit,
        skipped_write=result.skipped_write,
    )
