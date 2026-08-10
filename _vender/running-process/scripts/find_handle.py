#!/usr/bin/env python3
from __future__ import annotations

import argparse
from pathlib import Path

import psutil


def iter_matches(target: Path) -> list[str]:
    resolved = target.resolve()
    needle = str(resolved).lower()
    matches: list[str] = []

    for proc in psutil.process_iter(["pid", "name", "exe", "cmdline"]):
        try:
            memory_maps = proc.memory_maps()
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            memory_maps = []
        except Exception:
            memory_maps = []

        hit_sources: list[str] = []
        for mmap in memory_maps:
            path = getattr(mmap, "path", "")
            if path and path.lower() == needle:
                hit_sources.append("memory_map")
                break

        if not hit_sources:
            try:
                open_files = proc.open_files()
            except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
                open_files = []
            except Exception:
                open_files = []
            for opened in open_files:
                if opened.path.lower() == needle:
                    hit_sources.append("open_file")
                    break

        if hit_sources:
            cmdline = " ".join(proc.info.get("cmdline") or [])
            exe = proc.info.get("exe") or ""
            matches.append(
                f"pid={proc.pid} name={proc.info.get('name') or ''} "
                f"source={','.join(hit_sources)} exe={exe} cmdline={cmdline}"
            )

    return matches


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Find processes holding a file handle or module map"
    )
    parser.add_argument("path", help="Path to the target file")
    args = parser.parse_args()

    target = Path(args.path)
    matches = iter_matches(target)
    if not matches:
        print(f"no matches for {target.resolve()}")
        return 1

    print(f"matches for {target.resolve()}:")
    for match in matches:
        print(match)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
