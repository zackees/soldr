"""Dependency-free child output capture for repository scripts and PEP 517.

Capture into regular temporary files, never subprocess pipes. The direct
child's exit closes the wait even if a broker or daemon inherited its output
handles, and output volume cannot fill a pipe while the parent waits (#3528).
"""

from __future__ import annotations

import locale
import os
import subprocess
import tempfile
from contextlib import ExitStack
from typing import Any, BinaryIO, Literal, Sequence, overload


@overload
def _read_output(
    file: BinaryIO | None, *, text: Literal[False], encoding: str, errors: str
) -> bytes | None: ...


@overload
def _read_output(
    file: BinaryIO | None, *, text: Literal[True], encoding: str, errors: str
) -> str | None: ...


@overload
def _read_output(
    file: BinaryIO | None, *, text: bool, encoding: str, errors: str
) -> bytes | str | None: ...


def _read_output(
    file: BinaryIO | None, *, text: bool, encoding: str, errors: str
) -> bytes | str | None:
    if file is None:
        return None
    file.seek(0)
    data = file.read()
    if not text:
        return data
    return data.decode(encoding, errors).replace("\r\n", "\n").replace("\r", "\n")


def _input_file(
    stack: ExitStack, data: bytes | str, encoding: str, errors: str
) -> BinaryIO:
    file = stack.enter_context(tempfile.TemporaryFile())
    file.write(data.encode(encoding, errors) if isinstance(data, str) else data)
    file.seek(0)
    return file


def run_captured(
    args: Sequence[str | os.PathLike[str]] | str,
    *,
    capture_output: bool = False,
    capture_stdout: bool = False,
    capture_stderr: bool = False,
    **kwargs: Any,
) -> subprocess.CompletedProcess:
    """Run with subprocess-compatible results and file-backed capture.

    ``capture_stdout`` and ``capture_stderr`` replace explicit PIPE arguments.
    Input likewise comes from a regular file. Other subprocess options retain
    their meaning, including timeout, cwd, environment, and failure checking.
    """
    _reject_pipe_redirects(
        kwargs.get("stdin"), kwargs.get("stdout"), kwargs.get("stderr")
    )
    capture_stdout = capture_stdout or capture_output
    capture_stderr = capture_stderr or capture_output
    check = kwargs.pop("check", False)
    text = bool(
        kwargs.get("text")
        or kwargs.get("universal_newlines")
        or kwargs.get("encoding")
        or kwargs.get("errors")
    )
    encoding = kwargs.get("encoding") or locale.getpreferredencoding(False)
    errors = kwargs.get("errors") or "strict"
    data = kwargs.pop("input", None)
    with ExitStack() as stack:
        stdout = _capture_file(stack, kwargs.get("stdout"), "stdout", capture_stdout)
        stderr = _capture_file(stack, kwargs.get("stderr"), "stderr", capture_stderr)
        if stdout is not None:
            kwargs["stdout"] = stdout
        if stderr is not None:
            kwargs["stderr"] = stderr
        if data is not None:
            if kwargs.get("stdin") is not None:
                raise ValueError("stdin and input arguments may not both be used")
            kwargs["stdin"] = _input_file(stack, data, encoding, errors)
        try:
            result: subprocess.CompletedProcess[Any] = subprocess.run(
                args, check=False, **kwargs
            )
        except subprocess.TimeoutExpired as error:
            error.output = _read_output(
                stdout, text=False, encoding=encoding, errors=errors
            )
            error.stderr = _read_output(
                stderr, text=False, encoding=encoding, errors=errors
            )
            raise
        if stdout is not None:
            result.stdout = _read_output(
                stdout, text=text, encoding=encoding, errors=errors
            )
        if stderr is not None:
            result.stderr = _read_output(
                stderr, text=text, encoding=encoding, errors=errors
            )
    if check:
        result.check_returncode()
    return result


def _reject_pipe_redirects(*redirects: BinaryIO | int | None) -> None:
    if any(redirect == subprocess.PIPE for redirect in redirects):
        raise ValueError("PIPE is unsupported; use file capture or streaming")


def _capture_file(
    stack: ExitStack, existing: BinaryIO | int | None, name: str, enabled: bool
) -> BinaryIO | None:
    if not enabled:
        return None
    if existing is not None:
        raise ValueError(f"{name} and capture arguments may not both be used")
    return stack.enter_context(tempfile.TemporaryFile())


def checked_output(
    args: Sequence[str | os.PathLike[str]] | str, **kwargs: Any
) -> bytes | str:
    """Return stdout, raising CalledProcessError on a failed child."""
    if "input" in kwargs and kwargs["input"] is None:
        # check_output has historically treated explicit None as empty input,
        # while run(input=None) leaves stdin inherited.
        kwargs["input"] = b""
    return run_captured(args, capture_stdout=True, check=True, **kwargs).stdout
