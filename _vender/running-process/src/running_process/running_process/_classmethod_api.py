"""Classmethod / staticmethod API extracted from RunningProcess.

These free functions implement the bodies of ``run``, ``exec_script``,
``pseudo_terminal``, ``interactive_launch_spec``, ``interactive`` and
``run_streaming``. The class in :mod:`_core` keeps thin
static/classmethod delegators with identical signatures so the public
API is unchanged.
"""

from __future__ import annotations

import sys
import time
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any

from running_process.compat import (
    PIPE,
    STDOUT,
    CalledProcessError,
    CompletedProcess,
    TimeoutExpired,
    make_completed_process,
)
from running_process.expect import ExpectRule
from running_process.priority import CpuPriority
from running_process.pty import (
    Expect,
    IdleDetector,
    InteractiveLaunchSpec,
    InteractiveMode,
    InteractiveProcess,
    PseudoTerminalProcess,
)
from running_process.pty import (
    interactive_launch_spec as _interactive_launch_spec,
)
from running_process.running_process._helpers import (
    _parse_shebang_command,
    _safe_console_write,
)
from running_process.running_process._types import ProcessInfo

if TYPE_CHECKING:
    from running_process.running_process._core import RunningProcess


_BUFSIZE_NOT_SET = object()


def run(
    cls: type[RunningProcess],
    args: str | list[str],
    *,
    bufsize: int | object = _BUFSIZE_NOT_SET,
    executable: str | None = None,
    input: str | bytes | None = None,
    stdin: int | Any | None = None,
    stdout: int | Any | None = None,
    stderr: int | Any | None = None,
    capture_output: bool = False,
    shell: bool = False,
    cwd: str | Path | None = None,
    timeout: int | float | None = None,
    check: bool = False,
    encoding: str | None = None,
    errors: str | None = None,
    text: bool = True,
    env: dict[str, str] | None = None,
    universal_newlines: bool = False,
    on_timeout: Callable[[ProcessInfo], None] | None = None,
    raise_on_abnormal_exit: bool = False,
    nice: int | CpuPriority | None = None,
    **_other_popen_kwargs: Any,
) -> CompletedProcess[Any]:
    if input is not None and stdin is not None:
        raise ValueError("stdin and input arguments may not both be used.")

    if executable is not None:
        raise NotImplementedError("RunningProcess.run does not support executable= yet")
    if stdout not in (None, PIPE):
        raise NotImplementedError(
            "RunningProcess.run only supports stdout=None or PIPE"
        )
    if stderr not in (None, PIPE, STDOUT):
        raise NotImplementedError(
            "RunningProcess.run only supports stderr=None, PIPE, or STDOUT"
        )
    if bufsize is not _BUFSIZE_NOT_SET and bufsize != 1:
        raise NotImplementedError(
            "RunningProcess.run only supports default buffering or bufsize=1"
        )
    if _other_popen_kwargs:
        unsupported = ", ".join(sorted(_other_popen_kwargs))
        raise NotImplementedError(
            f"RunningProcess.run does not support extra Popen kwargs: {unsupported}"
        )
    should_text = (
        text or universal_newlines or encoding is not None or errors is not None
    )
    effective_stdin = PIPE if input is not None and stdin is None else stdin
    proc = cls(
        args,
        cwd=Path(cwd) if cwd is not None else None,
        shell=shell,
        timeout=int(timeout) if timeout is not None else None,
        capture=capture_output or stdout is PIPE or stderr is PIPE,
        env=env,
        stdin=effective_stdin,
        text=should_text,
        encoding=encoding,
        errors=errors,
        universal_newlines=universal_newlines,
        on_timeout=on_timeout,
        nice=nice,
        stderr=stderr,
    )
    if input is not None:
        payload = (
            input.encode(encoding or "utf-8", errors or "replace")
            if isinstance(input, str)
            else input
        )
        proc._proc.write_stdin(payload)
    try:
        returncode = proc.wait(
            timeout=timeout, raise_on_abnormal_exit=raise_on_abnormal_exit
        )
    except TimeoutError as exc:
        raise TimeoutExpired(args, timeout) from exc

    merged_output = capture_output or stdout is PIPE
    stdout_value: Any
    stderr_value: Any
    if merged_output:
        stdout_value = proc.combined_output if stderr in (None, STDOUT) else proc.stdout
    else:
        stdout_value = None
    stderr_value = proc.stderr if stderr is PIPE else None

    result: CompletedProcess[Any] = make_completed_process(
        args=args,
        returncode=returncode,
        stdout=stdout_value,
        stderr=stderr_value,
    )
    if check and result.returncode != 0:
        raise CalledProcessError(
            result.returncode,
            args,
            output=result.stdout,
            stderr=result.stderr,
        )
    return result


def exec_script(
    cls: type[RunningProcess],
    script: str | Path,
    *script_args: str,
    cwd: str | Path | None = None,
    timeout: int | float | None = None,
    check: bool = False,
    capture_output: bool = True,
    text: bool = True,
    env: dict[str, str] | None = None,
    nice: int | CpuPriority | None = None,
) -> CompletedProcess[Any]:
    script_path = Path(script)
    command = [*_parse_shebang_command(script_path), str(script_path), *script_args]
    effective_cwd = cwd
    if (
        effective_cwd is None
        and len(command) >= 3
        and command[0] == "uv"
        and command[1] == "run"
        and command[2] == "--script"
    ):
        effective_cwd = str(script_path.parent)
    return cls.run(
        command,
        cwd=effective_cwd,
        timeout=timeout,
        check=check,
        capture_output=capture_output,
        text=text,
        env=env,
        nice=nice,
    )


def pseudo_terminal(
    command: str | list[str],
    *,
    cwd: str | Path | None = None,
    shell: bool | None = None,
    env: dict[str, str] | None = None,
    capture: bool = True,
    text: bool = False,
    encoding: str = "utf-8",
    errors: str = "replace",
    rows: int = 24,
    cols: int = 80,
    nice: int | CpuPriority | None = None,
    restore_terminal: bool = True,
    auto_run: bool = True,
    expect: list[ExpectRule | Expect] | None = None,
    expect_timeout: float | None = None,
    idle_detector: IdleDetector | None = None,
    relay_terminal_input: bool = False,
    arm_idle_timeout_on_submit: bool = False,
) -> PseudoTerminalProcess:
    # Look up PseudoTerminalProcess on the _core module so monkeypatches that
    # target running_process.running_process._core.PseudoTerminalProcess (used
    # by existing tests, see tests/pty/test_pty_input_relay.py) take effect.
    from running_process.running_process import _core as _core_module

    pty_cls = _core_module.PseudoTerminalProcess
    registered_expect: list[Expect] = []
    bootstrap_expect: list[ExpectRule] = []
    if expect is not None:
        for rule in expect:
            if isinstance(rule, Expect):
                registered_expect.append(rule)
            else:
                bootstrap_expect.append(rule)
    process = pty_cls(
        command,
        cwd=cwd,
        shell=shell,
        env=env,
        capture=capture,
        text=text,
        encoding=encoding,
        errors=errors,
        rows=rows,
        cols=cols,
        nice=nice,
        restore_terminal=restore_terminal,
        expect=registered_expect or None,
        idle_detector=idle_detector,
        relay_terminal_input=relay_terminal_input,
        arm_idle_timeout_on_submit=arm_idle_timeout_on_submit,
        auto_run=auto_run,
    )
    if bootstrap_expect:
        for rule in bootstrap_expect:
            process.expect(rule.pattern, timeout=expect_timeout, action=rule.action)
    return process


def interactive_launch_spec(mode: InteractiveMode | str) -> InteractiveLaunchSpec:
    return _interactive_launch_spec(mode)


def interactive(
    cls: type[RunningProcess],
    command: str | list[str],
    *,
    mode: InteractiveMode | str = InteractiveMode.CONSOLE_SHARED,
    cwd: str | Path | None = None,
    shell: bool | None = None,
    env: dict[str, str] | None = None,
    text: bool = False,
    encoding: str = "utf-8",
    errors: str = "replace",
    rows: int = 24,
    cols: int = 80,
    nice: int | CpuPriority | None = None,
    restore_terminal: bool | None = None,
    auto_run: bool = True,
) -> InteractiveProcess | PseudoTerminalProcess:
    resolved_mode = InteractiveMode(mode)
    if resolved_mode is InteractiveMode.PSEUDO_TERMINAL:
        return cls.pseudo_terminal(
            command,
            cwd=cwd,
            shell=shell,
            env=env,
            text=text,
            encoding=encoding,
            errors=errors,
            rows=rows,
            cols=cols,
            nice=nice,
            restore_terminal=True if restore_terminal is None else restore_terminal,
            auto_run=auto_run,
        )
    return InteractiveProcess(
        command,
        mode=resolved_mode,
        cwd=cwd,
        shell=shell,
        env=env,
        nice=nice,
        restore_terminal=restore_terminal,
        auto_run=auto_run,
    )


def run_streaming(
    cls: type[RunningProcess],
    cmd: list[str],
    env: dict[str, str] | None = None,
    cwd: str | None = None,
    timeout: float | None = None,
    nice: int | CpuPriority | None = None,
    stdout_callback: Callable[[str], None] | None = None,
) -> int:
    process = cls(
        command=cmd,
        cwd=Path(cwd) if cwd is not None else None,
        env=env,
        timeout=int(timeout) if timeout is not None else None,
        nice=nice,
        auto_run=True,
    )
    deadline = time.time() + timeout if timeout is not None else None

    while True:
        code = process.poll()
        if stdout_callback is not None:
            for line in process.drain_stdout():
                text = (
                    line.decode("utf-8", errors="replace")
                    if isinstance(line, bytes)
                    else line
                )
                stdout_callback(text)
            for line in process.drain_stderr():
                _safe_console_write(sys.stderr, line)
        else:
            process._echo_streams()
        if code is not None:
            return code
        if deadline is not None and time.time() >= deadline:
            process._handle_timeout(timeout)
        # #199: intentional — wait-for-completion poll that
        # interleaves with the project's _handle_timeout machinery.
        # subprocess.Popen.wait(timeout) raises TimeoutExpired and
        # discards partial results; this loop keeps the result for
        # the caller's inspection on timeout.
        time.sleep(0.01)
