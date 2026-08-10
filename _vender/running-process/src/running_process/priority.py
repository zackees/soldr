from __future__ import annotations

from enum import Enum


class CpuPriority(str, Enum):
    MINIMAL = "minimal"
    LOW = "low"
    NORMAL = "normal"
    HIGH = "high"


def normalize_nice(nice: int | CpuPriority | None) -> int | None:
    if nice is None or isinstance(nice, int):
        return nice
    if not isinstance(nice, CpuPriority):
        raise TypeError("nice must be an int, CpuPriority, or None")
    return _priority_to_nice(nice)


def _priority_to_nice(priority: CpuPriority) -> int:
    if priority is CpuPriority.MINIMAL:
        return 15
    if priority is CpuPriority.LOW:
        return 5
    if priority is CpuPriority.NORMAL:
        return 0
    return -5
