from __future__ import annotations

from ci.claude_hooks import MANDATE_REASON
from ci.codex_hooks import pre_tool_use_response


def test_codex_pre_tool_use_denies_direct_raw_build_command() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build --workspace",
            },
        }
    )

    assert response == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": MANDATE_REASON,
        }
    }


def test_codex_pre_tool_use_allows_soldr_command() -> None:
    assert pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "soldr cargo test --workspace",
            },
        }
    ) is None


def test_codex_pre_tool_use_denies_compound_raw_build_command() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build && cargo test",
            },
        }
    )

    assert response == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": MANDATE_REASON,
        }
    }
