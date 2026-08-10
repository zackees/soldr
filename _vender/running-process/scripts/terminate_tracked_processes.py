from __future__ import annotations

from running_process.pid_tracker import cleanup_tracked_processes, tracked_pid_db_path


def main() -> int:
    killed = cleanup_tracked_processes()
    print(f"tracked_pid_db={tracked_pid_db_path()}")
    print(f"killed_processes={len(killed)}")
    for entry in killed:
        print(f"pid={entry.pid} kind={entry.kind} command={entry.command}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
