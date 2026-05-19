#!/usr/bin/env python3
"""PreToolUse hook: blocks bare Rust commands and bare python/pip.

All cargo/rustc/rustfmt must go through soldr (ensures correct toolchain).
All python must go through uv (ensures correct environment).

Exit codes:
  0 - Allow (outputs JSON hookSpecificOutput to deny if needed)
"""

import json
import re
import sys


RUST_TOOLS = {"cargo", "rustc", "rustfmt", "clippy-driver", "cargo-clippy", "cargo-fmt"}
PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}

SOLDR_PREFIXES = ("soldr ", "uv run soldr ")
UV_RUN_PREFIX = "uv run "
UV_PIP_PREFIX = "uv pip "
UV_RUN_FLAGS_WITH_VALUES = {
    "--config-setting",
    "--directory",
    "--env-file",
    "--extra",
    "--find-links",
    "--from",
    "--group",
    "--index-url",
    "--no-extra",
    "--no-group",
    "--only-group",
    "--project",
    "--python",
    "--with",
    "--with-editable",
    "--with-requirements",
    "-m",
    "-p",
}


def uv_run_target(parts):
    """Return the uv-run command target after leading uv options."""
    index = 2
    while index < len(parts):
        token = parts[index]
        if token == "--":
            index += 1
            continue
        if token in UV_RUN_FLAGS_WITH_VALUES:
            index += 2
            continue
        if any(token.startswith(f"{flag}=") for flag in UV_RUN_FLAGS_WITH_VALUES):
            index += 1
            continue
        if token.startswith("-"):
            index += 1
            continue
        return token
    return ""


def check_command(command):
    """Check a command string for forbidden bare invocations.

    Returns (tool, reason) if forbidden, None if allowed.
    """
    # ── Per-segment checks ───────────────────────────────────────────
    segments = re.split(r"&&|\|\||;", command)

    for seg in segments:
        seg = seg.strip()
        if not seg:
            continue

        # Skip if Rust tooling is explicitly routed through soldr.
        if any(seg.startswith(p) for p in SOLDR_PREFIXES):
            continue

        if seg.startswith(UV_PIP_PREFIX):
            continue

        first_word = seg.split()[0] if seg.split() else ""

        if seg.startswith(UV_RUN_PREFIX):
            parts = seg.split()
            # `uv run soldr ...` was handled above. Block the old `uv run cargo`
            # console-script shim path so Rust tooling has one canonical entry.
            run_target = uv_run_target(parts)
            if run_target in RUST_TOOLS:
                return (
                    run_target,
                    f"Use `uv run soldr {run_target} ...` instead of "
                    f"`uv run {run_target} ...`. soldr resolves the checked-in "
                    f"Rust toolchain directly via rustup.",
                )
            continue

        if first_word in RUST_TOOLS:
            return (
                first_word,
                f"Use `soldr {first_word} ...` or "
                f"`uv run soldr {first_word} ...` instead of bare "
                f"`{first_word}`. soldr resolves the checked-in Rust toolchain "
                f"directly via rustup.",
            )

        if first_word in PYTHON_TOOLS:
            if first_word.startswith("pip"):
                suggestion = f"uv pip {' '.join(seg.split()[1:])}" if len(seg.split()) > 1 else "uv pip ..."
                return (
                    first_word,
                    f"Use `{suggestion}` instead of bare `{first_word}`. "
                    f"All pip operations must go through uv.",
                )
            return (
                first_word,
                f"Use `uv run ...` instead of bare `{first_word}`. "
                f"All Python must be executed through uv.",
            )

    return None


def deny(reason):
    """Output a JSON deny response."""
    json.dump({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }, sys.stdout)


def extract_command(data):
    """Best-effort extraction across shell tool event shapes."""
    tool_input = data.get("tool_input", {})
    if not isinstance(tool_input, dict):
        return ""
    for key in ("command", "script", "cmd"):
        value = tool_input.get(key)
        if isinstance(value, str) and value.strip():
            return value
    return ""


def main():
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    tool_name = data.get("tool_name", "")
    if tool_name not in {"Bash", "Shell", "PowerShell"}:
        sys.exit(0)

    command = extract_command(data)
    if not command:
        sys.exit(0)

    result = check_command(command)
    if result:
        _, reason = result
        deny(reason)

    sys.exit(0)


if __name__ == "__main__":
    main()
