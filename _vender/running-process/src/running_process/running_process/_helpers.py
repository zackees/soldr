from __future__ import annotations

import re
import shlex
import time
from datetime import datetime, timezone
from io import TextIOBase
from pathlib import Path
from typing import Any

from running_process.compat import DEVNULL, PIPE
from running_process.expect import ExpectPattern
from running_process.running_process._types import EchoCallback, EchoValue


def _safe_console_write(stream: TextIOBase, line: EchoValue) -> None:
    text = line.decode("utf-8", errors="replace") if isinstance(line, bytes) else line
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


def _stdin_mode(stdin: int | Any | None, has_input: bool) -> str:
    if has_input:
        return "piped"
    if stdin is None:
        return "inherit"
    if stdin is DEVNULL:
        return "null"
    if stdin is PIPE:
        return "piped"
    raise ValueError("unsupported stdin value for RunningProcess; use None, PIPE, or DEVNULL")


def _validate_echo_flag(echo: bool | EchoCallback) -> None:
    if not isinstance(echo, bool) and not callable(echo):
        raise TypeError(f"echo must be bool or callable, got {type(echo).__name__}")


def _validate_echo_timestamps(echo_timestamps: str | None) -> None:
    if echo_timestamps is not None and echo_timestamps not in ("relative", "absolute"):
        raise ValueError(
            f"echo_timestamps must be None, 'relative', or 'absolute', got {echo_timestamps!r}"
        )


def _make_timestamped_callback(
    inner: EchoCallback,
    mode: str,
    start_time: float,
) -> EchoCallback:
    if mode == "relative":

        def _relative_cb(line: str) -> None:
            elapsed = time.time() - start_time
            inner(f"[{elapsed:.2f}] {line}")

        return _relative_cb

    def _absolute_cb(line: str) -> None:
        stamp = datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3]
        inner(f"[{stamp}] {line}")

    return _absolute_cb


def _parse_shebang_command(script_path: Path) -> list[str]:
    first_line = script_path.read_text(encoding="utf-8", errors="replace").splitlines()[0]
    if first_line.startswith("\ufeff"):
        first_line = first_line.removeprefix("\ufeff")
    if not first_line.startswith("#!"):
        raise ValueError(f"Script does not start with a shebang: {script_path}")

    parts = shlex.split(first_line[2:].strip(), posix=True)
    if not parts:
        raise ValueError(f"Invalid shebang in script: {script_path}")

    interpreter = parts[0]
    if ("/" in interpreter or "\\" in interpreter) and not Path(interpreter).exists():
        parts[0] = Path(interpreter).name

    if Path(parts[0]).name == "env":
        env_args = parts[1:]
        if env_args and env_args[0] in {"-S", "--split-string"}:
            env_args = env_args[1:]
        if not env_args:
            raise ValueError(f"Shebang env launcher has no command: {script_path}")
        parts = env_args

    return parts


def _validate_expect_stream(stream: str) -> str:
    if stream not in {"stdout", "stderr", "combined"}:
        raise ValueError("stream must be 'stdout', 'stderr', or 'combined'")
    return stream


def _expect_pattern_spec(pattern: ExpectPattern) -> tuple[str, bool]:
    if isinstance(pattern, str):
        return pattern, False
    if isinstance(pattern, re.Pattern):
        return pattern.pattern, True
    raise TypeError("pattern must be a string or compiled regex pattern")
