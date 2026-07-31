#!/usr/bin/env -S uv run --no-project --script
"""PreToolUse hook: blocks bare Rust commands and bare python/pip.

All Rust toolchain commands (cargo, rustup, rustc, rustfmt, clippy-driver,
cargo-clippy, cargo-fmt, rustdoc, rust-gdb, rust-lldb, rust-analyzer) must
go through soldr. soldr resolves the project-pinned toolchain via rustup
and ensures every per-unit compile is routed through the soldr-managed
zccache.

All python must go through uv (ensures correct environment).

Leading env-var assignments (`FOO=bar baz=qux cargo build`) are stripped
before evaluation so they cannot be used as a backdoor around the policy.

Exit codes:
  0 - Allow (outputs JSON hookSpecificOutput to deny if needed)
"""

import json
import re
import shlex
import sys

# Anything in this set, invoked bare, is denied. The user is expected to
# route through `soldr <tool> ...` (or `uv run --no-project soldr <tool> ...`).
RUST_TOOLS = {
    "cargo",
    "cargo-clippy",
    "cargo-fmt",
    "clippy-driver",
    "rustc",
    "rustdoc",
    "rustfmt",
    "rustup",
    "rust-gdb",
    "rust-lldb",
    "rust-analyzer",
}

PYTHON_TOOLS = {"python", "python3", "pip", "pip3"}
SHELL_TOOL_NAMES = {
    "Bash",
    "Shell",
    "PowerShell",
    "shell_command",
    "functions.shell_command",
}

# Shell control operators that end one top-level command and start another.
# `|` is intentionally absent — the hook has never split on pipes (a bare
# `cargo build | tee log` should still be blocked because `cargo` is the
# head of the pipeline; `grep foo | cargo install` slipping through is a
# pre-existing gap the shlex switch deliberately does not change).
SEGMENT_OPERATORS = {";", ";;", "&&", "||"}

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
UV_RUN_REBUILD_GUARD_FLAGS = {"--no-project", "--no-sync", "--frozen"}

# Matches `IDENT=`, with optional digits/underscores after the first letter.
# Used to strip leading shell env-var assignments before evaluating the real
# command, so `RUSTUP_TOOLCHAIN=foo cargo build` is recognized as `cargo build`.
ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def strip_env_prefix(tokens):
    """Return tokens with leading shell env-var assignments removed."""
    i = 0
    while i < len(tokens) and ENV_ASSIGN_RE.match(tokens[i]):
        i += 1
    return tokens[i:]


def tokenize_segments(command):
    """Yield the token list for each top-level shell segment in `command`.

    Uses `shlex` with `punctuation_chars=';&|'` so unquoted `;`, `&&`,
    `||` (and pipe runs) emerge as their own tokens while content
    inside single or double quotes stays in one token. That means
    `bash -c 'cargo build'` is a single segment headed by `bash` —
    the inner `cargo` lives inside a quoted argument and is invisible
    to the policy (it never reaches the host shell as a bare command).

    Malformed quoting falls back to the old regex behavior so the
    hook errs on the side of inspecting more, not less.
    """

    try:
        lex = shlex.shlex(command, posix=True, punctuation_chars=";&|")
        lex.whitespace_split = True
        tokens = list(lex)
    except ValueError:
        # Unclosed quote / other shlex parse error: fall back to the
        # legacy split-on-operators behavior so we still inspect each
        # rough segment instead of letting the whole command through.
        tokens = []
        for raw_seg in re.split(r"&&|\|\||;", command):
            tokens.extend(raw_seg.split())
            tokens.append(";")
        if tokens and tokens[-1] == ";":
            tokens.pop()

    current = []
    for token in tokens:
        if token in SEGMENT_OPERATORS:
            if current:
                yield current
            current = []
        else:
            current.append(token)
    if current:
        yield current


def uv_run_target(parts):
    """Return the uv-run command target after leading uv options.

    `parts` is the token list AFTER any env-var prefix has been stripped,
    starting with `uv run ...`.
    """
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


def uv_run_has_rebuild_guard(parts):
    """Return True if `uv run` itself carries a no-rebuild guard flag."""
    index = 2
    while index < len(parts):
        token = parts[index]
        if token == "--":
            return False
        if token in UV_RUN_REBUILD_GUARD_FLAGS:
            return True
        if token in UV_RUN_FLAGS_WITH_VALUES:
            index += 2
            continue
        if any(token.startswith(f"{flag}=") for flag in UV_RUN_FLAGS_WITH_VALUES):
            index += 1
            continue
        if token.startswith("-"):
            index += 1
            continue
        return False
    return False


def check_command(command):
    """Check a command string for forbidden bare invocations.

    Returns (tool, reason) if forbidden, None if allowed.
    """
    for segment_tokens in tokenize_segments(command):
        tokens = strip_env_prefix(segment_tokens)
        if not tokens:
            continue

        # `soldr <tool>` is the canonical direct entry point.
        if tokens[0] == "soldr":
            continue

        # `uv pip ...` is the canonical pip replacement — allow.
        if tokens[:2] == ["uv", "pip"]:
            continue

        first_word = tokens[0]

        # `uv run <something>` must opt out of auto-sync before it can
        # reach the project. The old `uv run cargo` console-script shim
        # remains blocked so Rust tooling has one canonical entry point.
        if tokens[:2] == ["uv", "run"]:
            run_target = uv_run_target(tokens)
            if run_target in RUST_TOOLS:
                return (
                    run_target,
                    f"Use `uv run --no-project soldr {run_target} ...` instead of "
                    f"`uv run {run_target} ...`. soldr resolves the project-pinned "
                    f"Rust toolchain via rustup.",
                )
            if not uv_run_has_rebuild_guard(tokens):
                return (
                    "uv run",
                    "Use `uv run --no-project ...`, `uv run --no-sync ...`, "
                    "or `uv run --frozen ...` instead of unguarded `uv run ...`. "
                    "Unguarded uv run can auto-sync this maturin project and "
                    "silently rebuild the native extension before the requested "
                    "dev-loop command starts.",
                )
            continue

        if first_word in RUST_TOOLS:
            return (
                first_word,
                f"Use `soldr {first_word} ...` or "
                f"`uv run --no-project soldr {first_word} ...` instead of bare "
                f"`{first_word}`. All Rust toolchain commands (cargo, rustup, "
                f"rustc, rustfmt, clippy-driver, cargo-clippy, cargo-fmt, "
                f"rustdoc, rust-gdb, rust-lldb, rust-analyzer) must go through "
                f"soldr -- including invocations with leading env-var "
                f"assignments like `FOO=bar {first_word} ...`.",
            )

        if first_word in PYTHON_TOOLS:
            if first_word.startswith("pip"):
                suggestion = (
                    f"uv pip {' '.join(tokens[1:])}"
                    if len(tokens) > 1
                    else "uv pip ..."
                )
                return (
                    first_word,
                    f"Use `{suggestion}` instead of bare `{first_word}`. "
                    f"All pip operations must go through uv.",
                )
            return (
                first_word,
                f"Use `uv run --no-project ...` instead of bare `{first_word}`. "
                f"All Python must be executed through uv.",
            )

    return None


def deny(reason):
    """Output a JSON deny response."""
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        },
        sys.stdout,
    )


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
        data = json.loads(sys.stdin.read().lstrip("\ufeff"))
    except json.JSONDecodeError:
        sys.exit(0)

    tool_name = data.get("tool_name", "")
    if tool_name not in SHELL_TOOL_NAMES:
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
