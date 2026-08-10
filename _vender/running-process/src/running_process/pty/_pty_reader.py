"""Pseudo-terminal output reading and pump helpers.

Free-function bodies extracted from ``PseudoTerminalProcess`` so the
parent module stays under its size budget. Public access remains via
``PseudoTerminalProcess`` — these helpers are not part of the public
API.
"""

from __future__ import annotations

import time
from typing import TYPE_CHECKING

from running_process.expect import ensure_text

if TYPE_CHECKING:
    from running_process.pty._pseudo_terminal import PseudoTerminalProcess


_PTY_READ_CHUNK_TIMEOUT_SECONDS = 0.01


def read(process: PseudoTerminalProcess, timeout: float | None = None) -> str | bytes:
    if not process.capture:
        raise NotImplementedError("PTY read() requires capture=True")
    chunk = process.read_non_blocking()
    if chunk is not None:
        return chunk
    _, stream_closed = pump_native_output(process, timeout=timeout, consume_all=False)
    chunk = process.read_non_blocking()
    if chunk is not None:
        return chunk
    if stream_closed or process._native_stream_closed:
        raise EOFError("Pseudo-terminal stream is closed")
    raise TimeoutError("No pseudo-terminal output available before timeout")


def read_non_blocking(process: PseudoTerminalProcess) -> str | bytes | None:
    if not process.capture:
        raise NotImplementedError("PTY read_non_blocking() requires capture=True")
    pump_native_output(process, timeout=0.0, consume_all=True)
    try:
        assert process._buffer is not None
        return process._buffer.read_non_blocking()
    except RuntimeError as exc:
        if "stream is closed" in str(exc):
            raise EOFError("Pseudo-terminal stream is closed") from exc
        raise


def read_text(process: PseudoTerminalProcess, timeout: float | None = None) -> str:
    """Like ``read()`` but always returns ``str``, decoded and sanitized for the parent console.

    Use this when the result will be printed to ``sys.stdout``: the value is
    round-tripped through the auto-detected console encoding with
    ``errors='replace'``, so writing it to a cp1252 console will not raise
    ``UnicodeEncodeError`` even when the child emitted UTF-8.
    """
    from running_process.console_encoding import sanitize_for_encoding

    chunk = process.read(timeout=timeout)
    if isinstance(chunk, bytes):
        chunk = chunk.decode(process.encoding, process.errors)
    return sanitize_for_encoding(chunk, process.encoding)


def drain(process: PseudoTerminalProcess) -> list[str | bytes]:
    if not process.capture:
        raise NotImplementedError("PTY drain() requires capture=True")
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return process._buffer.drain()


def drain_echo(process: PseudoTerminalProcess) -> list[bytes]:
    pump_native_output(process, timeout=0.0, consume_all=True)
    chunks = list(process._pending_echo_chunks)
    process._pending_echo_chunks.clear()
    return chunks


def discard_output(process: PseudoTerminalProcess) -> int:
    if not process.capture:
        pump_native_output(process, timeout=0.0, consume_all=True)
        return 0
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return int(process._buffer.clear_history())


def output_bytes(process: PseudoTerminalProcess) -> int:
    if not process.capture:
        pump_native_output(process, timeout=0.0, consume_all=True)
        return 0
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return int(process._buffer.history_bytes())


def output_since(process: PseudoTerminalProcess, start: int) -> str | bytes:
    if not process.capture:
        raise NotImplementedError("PTY output capture is disabled")
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return process._buffer.output_since(max(0, start))


def snapshot_output_history(process: PseudoTerminalProcess) -> tuple[str, int]:
    if not process.capture:
        raise NotImplementedError("PTY output capture is disabled")
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return (
        ensure_text(process._buffer.output(), process.encoding, process.errors),
        int(process._buffer.history_bytes()),
    )


def snapshot_output_since(
    process: PseudoTerminalProcess, start: int
) -> tuple[str, int]:
    if not process.capture:
        raise NotImplementedError("PTY output capture is disabled")
    pump_native_output(process, timeout=0.0, consume_all=True)
    assert process._buffer is not None
    return (
        ensure_text(
            process._buffer.output_since(max(0, start)),
            process.encoding,
            process.errors,
        ),
        int(process._buffer.history_bytes()),
    )


def read_chunk(
    process: PseudoTerminalProcess,
    *,
    timeout: float | None = None,
) -> bytes | None:
    try:
        assert process._proc is not None
        wait_timeout = _PTY_READ_CHUNK_TIMEOUT_SECONDS if timeout is None else timeout
        return process._proc.read_chunk(timeout=wait_timeout)
    except TimeoutError:
        return None
    except RuntimeError as exc:
        if "stream is closed" in str(exc):
            return b""
        raise


def pump_native_output(
    process: PseudoTerminalProcess,
    *,
    timeout: float | None,
    consume_all: bool,
) -> tuple[bool, bool]:
    if process._proc is None or process._native_stream_closed:
        return False, process._native_stream_closed
    read_any = False
    wait_timeout = timeout
    while True:
        chunk = read_chunk(process, timeout=wait_timeout)
        if chunk is None:
            return read_any, False
        if not chunk:
            process._mark_native_stream_closed()
            return read_any, True
        process._handle_native_chunk(chunk)
        read_any = True
        if not consume_all:
            return read_any, False
        wait_timeout = 0.0


def drain_native_until_eof(process: PseudoTerminalProcess, *, timeout: float) -> None:
    if process._proc is None or process._native_stream_closed:
        return
    deadline = time.monotonic() + max(0.0, timeout)
    first_wait = True
    while not process._native_stream_closed:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        wait_timeout = remaining if first_wait else min(0.05, remaining)
        first_wait = False
        pump_native_output(process, timeout=wait_timeout, consume_all=True)
    watcher_thread = process._native_exit_watcher
    if watcher_thread is not None:
        watcher_thread.join(timeout=2)
