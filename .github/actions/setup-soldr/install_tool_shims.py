#!/usr/bin/env python3
from __future__ import annotations

import os
import shutil
import stat
import subprocess
from pathlib import Path


TOOL_GROUPS = {
    "cargo": ["cargo"],
    "rust": ["cargo", "rustc", "rustfmt", "clippy-driver", "rustdoc"],
    "all": ["cargo", "rustc", "rustfmt", "clippy-driver", "rustdoc"],
}
DISABLED_VALUES = {"", "0", "false", "no", "none", "off"}


def _write_env(name: str, value: str) -> None:
    output = os.environ.get("GITHUB_ENV")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{name}={value}\n")


def _write_path(value: str) -> None:
    output = os.environ.get("GITHUB_PATH")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as fh:
        fh.write(f"{value}\n")


def tool_env_name(tool: str) -> str:
    cleaned = "".join(ch.upper() if ch.isalnum() else "_" for ch in tool)
    return f"SOLDR_REAL_{cleaned}"


def parse_requested_tools(value: str) -> list[str]:
    normalized = value.strip().lower()
    if normalized in DISABLED_VALUES:
        return []

    tools: list[str] = []
    for part in value.split(","):
        token = part.strip().lower()
        if not token:
            continue
        expanded = TOOL_GROUPS.get(token, [token])
        for tool in expanded:
            if tool not in tools:
                tools.append(tool)
    return tools


def resolve_tool(tool: str) -> str:
    rustup = shutil.which("rustup")
    if rustup:
        result = subprocess.run(
            [rustup, "which", tool],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip()

    path = shutil.which(tool)
    if path:
        return path

    raise RuntimeError(f"setup-soldr could not resolve requested tool shim target: {tool}")


def write_unix_shim(path: Path, soldr_path: str, tool: str) -> None:
    path.write_text(
        f"#!/bin/sh\nexec {sh_quote(soldr_path)} {sh_quote(tool)} \"$@\"\n",
        encoding="utf-8",
    )
    path.chmod(path.stat().st_mode | stat.S_IEXEC)


def write_windows_shim(path: Path, soldr_path: str, tool: str) -> None:
    path.write_text(
        f"@echo off\r\n\"{soldr_path}\" {tool} %*\r\n",
        encoding="utf-8",
    )


def sh_quote(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


def install_shims(shim_dir: Path, soldr_path: str, tools: list[str]) -> None:
    shim_dir.mkdir(parents=True, exist_ok=True)
    for tool in tools:
        real_tool = resolve_tool(tool)
        _write_env(tool_env_name(tool), real_tool)

        if os.name == "nt":
            write_windows_shim(shim_dir / f"{tool}.cmd", soldr_path, tool)
        else:
            write_unix_shim(shim_dir / tool, soldr_path, tool)

    if tools:
        _write_path(str(shim_dir))


def main() -> None:
    tools = parse_requested_tools(os.environ.get("SETUP_SOLDR_TOOL_SHIMS", "false"))
    if not tools:
        return

    soldr_path = os.environ["SETUP_SOLDR_PATH"]
    shim_dir = Path(os.environ["SETUP_SOLDR_SHIM_DIR"])
    install_shims(shim_dir, soldr_path, tools)


if __name__ == "__main__":
    main()
