"""Where diagnostic artifacts go, and what they are named.

Extracted so the CLI supervisor and the in-process probe agree on one
convention. They produce *different* artifacts about *different* processes —
see below — but an operator collecting evidence should find all of it in one
place, named the same way.
"""

from __future__ import annotations

import os
from datetime import UTC, datetime
from pathlib import Path

RUNNING_PROCESS_STACK_DUMP_DIR_ENV = "RUNNING_PROCESS_STACK_DUMP_DIR"


def stack_dump_dir(override: Path | None = None) -> Path:
    """Directory for diagnostic artifacts.

    An explicit argument wins, then the environment, then a path under the
    working directory.
    """
    if override is not None:
        return override
    configured = os.environ.get(RUNNING_PROCESS_STACK_DUMP_DIR_ENV)
    if configured:
        return Path(configured)
    return Path.cwd() / "logs" / "running-process"


def artifact_stem(*, reason: str, pid: int | None) -> str:
    """Filename stem shared by every artifact from one dump.

    The pid is part of the name because a dump describes one specific
    process, and a directory collects dumps from many.
    """
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    pid_part = str(pid) if pid is not None else "unknown"
    return f"{timestamp}-pid{pid_part}-{reason}"


def utc_now_iso() -> str:
    """Timestamp for artifact metadata."""
    return datetime.now(UTC).isoformat()
