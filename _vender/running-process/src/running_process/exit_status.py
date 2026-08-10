from __future__ import annotations

import signal
import sys
from dataclasses import dataclass

WINDOWS_NO_MEMORY_CODES = {
    -1073741801,  # signed 0xC0000017
    3221225495,  # unsigned 0xC0000017
}


@dataclass(frozen=True)
class ExitStatus:
    returncode: int
    abnormal: bool
    interrupted: bool
    signal_number: int | None
    signal_name: str | None
    possible_oom: bool
    summary: str


class ProcessAbnormalExit(RuntimeError):  # noqa: N818
    def __init__(self, status: ExitStatus) -> None:
        super().__init__(status.summary)
        self.status = status


def classify_exit_status(
    returncode: int, interrupted_codes: set[int], platform: str | None = None
) -> ExitStatus:
    platform = platform or sys.platform
    interrupted = returncode in interrupted_codes
    signal_number: int | None = None
    signal_name: str | None = None
    possible_oom = False

    if platform != "win32" and returncode < 0:
        signal_number = -returncode
        signal_name = _signal_name(signal_number)
        possible_oom = signal_number == getattr(signal, "SIGKILL", 9)

    if platform == "win32" and returncode in WINDOWS_NO_MEMORY_CODES:
        possible_oom = True

    abnormal = returncode != 0 and not interrupted
    summary = _summary(
        returncode=returncode,
        abnormal=abnormal,
        interrupted=interrupted,
        signal_number=signal_number,
        signal_name=signal_name,
        possible_oom=possible_oom,
    )
    return ExitStatus(
        returncode=returncode,
        abnormal=abnormal,
        interrupted=interrupted,
        signal_number=signal_number,
        signal_name=signal_name,
        possible_oom=possible_oom,
        summary=summary,
    )


def _signal_name(number: int) -> str | None:
    try:
        return signal.Signals(number).name
    except ValueError:
        return None


def _summary(
    *,
    returncode: int,
    abnormal: bool,
    interrupted: bool,
    signal_number: int | None,
    signal_name: str | None,
    possible_oom: bool,
) -> str:
    if interrupted:
        return f"Process interrupted (return code {returncode})"
    if signal_number is not None:
        signal_display = signal_name or f"signal {signal_number}"
        if possible_oom:
            return (
                f"Process exited from {signal_display} (return code {returncode}); "
                "this may indicate a forced kill or OOM condition"
            )
        return f"Process exited from {signal_display} (return code {returncode})"
    if possible_oom:
        return (
            f"Process exited abnormally with return code {returncode}; "
            "this may indicate an out-of-memory condition"
        )
    if abnormal:
        return f"Process exited abnormally with return code {returncode}"
    return f"Process exited normally with return code {returncode}"
