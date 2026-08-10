from __future__ import annotations

import shlex
import sys
from contextlib import suppress

from running_process._native import native_apply_process_nice
from running_process.command_render import list2cmdline as render_command_list
from running_process.compat import CREATE_NEW_PROCESS_GROUP
from running_process.pty._types import InteractiveLaunchSpec, InteractiveMode


def _windows_pty_command(command: str | list[str], shell: bool) -> list[str]:
    if shell:
        if isinstance(command, str):
            return ["cmd", "/C", command]
        return ["cmd", "/C", render_command_list(command)]
    if isinstance(command, str):
        return [command]
    return command


def _posix_pty_command(
    command: str | list[str], shell: bool, nice: int | None = None
) -> list[str]:
    if shell:
        if isinstance(command, str):
            argv = ["sh", "-lc", command]
        else:
            argv = ["sh", "-lc", shlex.join(command)]
    elif isinstance(command, str):
        argv = [command]
    else:
        argv = command
    if nice is None:
        return argv
    return _wrap_posix_pty_command_with_nice(argv, nice)


def _wrap_posix_pty_command_with_nice(argv: list[str], nice: int) -> list[str]:
    return [
        sys.executable,
        "-c",
        (
            "import os, sys\n"
            "os.setpriority(os.PRIO_PROCESS, 0, int(sys.argv[1]))\n"
            "os.execvp(sys.argv[2], sys.argv[2:])\n"
        ),
        str(nice),
        *argv,
    ]


def _pty_command(command: str | list[str], shell: bool, nice: int | None = None) -> list[str]:
    if sys.platform == "win32":
        return _windows_pty_command(command, shell)
    return _posix_pty_command(command, shell, nice)


def _normalize_command(
    command: str | list[str], shell: bool | None
) -> tuple[str | list[str], bool]:
    if isinstance(command, list):
        return command, bool(shell)

    if shell is True:
        return command, True

    if shell is False:
        return _split_command(command), False

    if _contains_shell_metacharacters(command):
        return command, True
    return _split_command(command), False


def _contains_shell_metacharacters(command: str) -> bool:
    shell_meta = {"&&", "||", "|", ";", ">", "<", "&"}
    return any(token in command for token in shell_meta)


def _split_command(command: str) -> list[str]:
    parts = shlex.split(command, posix=False)
    return [_strip_wrapping_quotes(part) for part in parts]


def _apply_process_nice(pid: int | None, nice: int | None) -> None:
    if pid is None or nice is None:
        return
    with suppress(RuntimeError):
        native_apply_process_nice(pid, nice)


def _strip_wrapping_quotes(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        return value[1:-1]
    return value


def interactive_launch_spec(mode: InteractiveMode | str) -> InteractiveLaunchSpec:
    resolved = InteractiveMode(mode)
    if resolved is InteractiveMode.PSEUDO_TERMINAL:
        return InteractiveLaunchSpec(
            mode=resolved,
            uses_pty=True,
            ctrl_c_owner="child",
            creationflags=None,
            restore_terminal=True,
        )
    if resolved is InteractiveMode.CONSOLE_ISOLATED:
        return InteractiveLaunchSpec(
            mode=resolved,
            uses_pty=False,
            ctrl_c_owner="parent",
            creationflags=CREATE_NEW_PROCESS_GROUP if sys.platform == "win32" else None,
            restore_terminal=True,
        )
    return InteractiveLaunchSpec(
        mode=resolved,
        uses_pty=False,
        ctrl_c_owner="shared",
        creationflags=None,
        restore_terminal=False,
    )
