from __future__ import annotations

import sys
import threading
from io import TextIOBase

_WINDOWS_VT_OUTPUT_HANDLES: set[int] = set()
_WINDOWS_VT_OUTPUT_LOCK = threading.Lock()


def _safe_console_write(stream: TextIOBase, line: str | bytes) -> None:
    text = line.decode("utf-8", errors="replace") if isinstance(line, bytes) else line
    _ensure_windows_vt_output(stream)
    try:
        stream.write(text)
        stream.write("\n")
    except UnicodeEncodeError:
        encoding = stream.encoding or "utf-8"
        rendered = text.encode(encoding, errors="replace")
        if hasattr(stream, "buffer"):
            stream.buffer.write(rendered + b"\n")
        else:
            stream.write(rendered.decode(encoding, errors="replace"))
            stream.write("\n")
    stream.flush()


def _windows_console_output_handle(stream: TextIOBase) -> int | None:
    if sys.platform != "win32":
        return None
    try:
        fileno = stream.fileno()
    except (AttributeError, OSError, ValueError):
        return None
    try:
        import msvcrt
    except ImportError:
        return None
    try:
        return int(msvcrt.get_osfhandle(fileno))
    except OSError:
        return None


def _enable_windows_vt_output_handle(handle: int) -> bool:
    if sys.platform != "win32":
        return False
    try:
        import ctypes
    except ImportError:
        return False

    kernel32 = ctypes.windll.kernel32
    mode = ctypes.c_uint32()
    console_handle = ctypes.c_void_p(handle)
    if kernel32.GetConsoleMode(console_handle, ctypes.byref(mode)) == 0:
        return False

    enable_processed_output = 0x0001
    enable_virtual_terminal_processing = 0x0004
    updated_mode = (
        mode.value | enable_processed_output | enable_virtual_terminal_processing
    )
    if updated_mode == mode.value:
        return True
    return kernel32.SetConsoleMode(console_handle, updated_mode) != 0


def _ensure_windows_vt_output(stream: TextIOBase) -> None:
    handle = _windows_console_output_handle(stream)
    if handle is None:
        return
    with _WINDOWS_VT_OUTPUT_LOCK:
        if handle in _WINDOWS_VT_OUTPUT_HANDLES:
            return
        if _enable_windows_vt_output_handle(handle):
            _WINDOWS_VT_OUTPUT_HANDLES.add(handle)


def _safe_console_write_chunk(
    stream: TextIOBase,
    chunk: bytes,
    *,
    encoding: str,
    errors: str,
) -> None:
    if not chunk:
        return
    _ensure_windows_vt_output(stream)
    if hasattr(stream, "buffer"):
        try:
            stream.buffer.write(chunk)
            stream.flush()
            return
        except UnicodeEncodeError:
            pass
    text = chunk.decode(encoding, errors)
    try:
        stream.write(text)
    except UnicodeEncodeError:
        fallback_encoding = stream.encoding or encoding or "utf-8"
        rendered = text.encode(fallback_encoding, errors="replace")
        if hasattr(stream, "buffer"):
            stream.buffer.write(rendered)
        else:
            stream.write(rendered.decode(fallback_encoding, errors="replace"))
    stream.flush()
