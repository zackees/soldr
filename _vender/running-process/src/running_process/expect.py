from __future__ import annotations

from dataclasses import dataclass
from re import Pattern

ExpectPattern = str | Pattern[str]
ExpectAction = str | bytes | None


@dataclass
class ExpectMatch:
    buffer: str
    matched: str
    span: tuple[int, int]
    groups: tuple[str, ...]


@dataclass
class ExpectRule:
    pattern: ExpectPattern
    action: ExpectAction = None


def search_expect_pattern(buffer: str, pattern: ExpectPattern) -> ExpectMatch | None:
    if isinstance(pattern, str):
        index = buffer.find(pattern)
        if index == -1:
            return None
        return ExpectMatch(
            buffer=buffer,
            matched=pattern,
            span=(index, index + len(pattern)),
            groups=(),
        )

    match = pattern.search(buffer)
    if match is None:
        return None
    return ExpectMatch(
        buffer=buffer,
        matched=match.group(0),
        span=match.span(),
        groups=match.groups(),
    )


def apply_expect_action(process: object, action: ExpectAction, match: ExpectMatch) -> None:
    if action is None:
        return
    if isinstance(action, bytes):
        process.write(action)  # type: ignore[attr-defined]
        return
    if action == "terminate":
        process.terminate()  # type: ignore[attr-defined]
        return
    if action == "kill":
        process.kill()  # type: ignore[attr-defined]
        return
    if action == "interrupt":
        if hasattr(process, "send_interrupt"):
            process.send_interrupt()  # type: ignore[attr-defined]
            return
        process.terminate()  # type: ignore[attr-defined]
        return
    process.write(action)  # type: ignore[attr-defined]


def ensure_text(value: str | bytes, encoding: str = "utf-8", errors: str = "replace") -> str:
    if isinstance(value, str):
        return value
    return value.decode(encoding, errors)
