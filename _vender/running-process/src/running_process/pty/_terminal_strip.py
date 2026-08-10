from __future__ import annotations

import re
from dataclasses import dataclass, field


@dataclass(slots=True)
class _TerminalControlStripper:
    mode: str = "capture"
    _pending: bytearray = field(default_factory=bytearray)
    _string_terminator: bytes | None = None

    def strip(self, chunk: bytes) -> bytes:
        if not chunk and not self._pending:
            return b""
        data = bytes(self._pending) + bytes(chunk)
        self._pending.clear()
        output = bytearray()
        index = 0

        while index < len(data):
            if self._string_terminator is not None:
                terminator = self._string_terminator
                terminator_index = data.find(terminator, index)
                if terminator_index == -1:
                    self._pending.extend(data[index:])
                    break
                index = terminator_index + len(terminator)
                self._string_terminator = None
                continue

            byte = data[index]
            if byte == 0x1B:
                if index + 1 >= len(data):
                    self._pending.append(byte)
                    break
                marker = data[index + 1]
                if marker == ord("["):
                    end = _find_csi_end(data, index + 2)
                    if end is None:
                        self._pending.extend(data[index:])
                        break
                    normalized = _normalize_csi_sequence(
                        data[index : end + 1],
                        mode=self.mode,
                    )
                    if normalized:
                        output.extend(normalized)
                    index = end + 1
                    continue
                if marker == ord("]"):
                    self._string_terminator = b"\x07"
                    index += 2
                    continue
                if marker in {ord("P"), ord("X"), ord("^"), ord("_")}:
                    self._string_terminator = b"\x1b\\"
                    index += 2
                    continue
                index += 2
                continue
            if byte in {0x08, 0x7F}:
                index += 1
                continue
            output.append(byte)
            index += 1

        return bytes(output)


def _find_csi_end(data: bytes, start: int) -> int | None:
    for index in range(start, len(data)):
        current = data[index]
        if 0x40 <= current <= 0x7E:
            return index
    return None


_ECHO_DISPLAY_CSI_FINALS = {
    b"A",
    b"B",
    b"C",
    b"D",
    b"E",
    b"F",
    b"G",
    b"H",
    b"J",
    b"K",
    b"S",
    b"T",
    b"f",
    b"m",
    b"s",
    b"u",
}


def _normalize_csi_sequence(chunk: bytes, *, mode: str) -> bytes:
    if len(chunk) < 3 or not chunk.startswith(b"\x1b["):
        return b""
    final = chunk[-1:]
    params = chunk[2:-1]
    if mode == "echo":
        if final in _ECHO_DISPLAY_CSI_FINALS:
            return b"\x1b[0m" if final == b"m" and params == b"" else chunk
        if final in {b"h", b"l"}:
            return chunk
        return b""
    if final == b"H" and params in {b"", b"1;1"}:
        return b"\r"
    if final == b"G" and params in {b"", b"1"}:
        return b"\r"
    return b""


_TERMINAL_FRAGMENT_RE = re.compile(rb"(?:\d+;){2,}\d+_")


def _strip_terminal_fragments(chunk: bytes) -> bytes:
    if not chunk:
        return chunk
    return _TERMINAL_FRAGMENT_RE.sub(b"", chunk)


def _collapse_duplicate_carriage_returns(chunk: bytes) -> bytes:
    if not chunk:
        return chunk
    while b"\r\r\n" in chunk:
        chunk = chunk.replace(b"\r\r\n", b"\r\n")
    return chunk
