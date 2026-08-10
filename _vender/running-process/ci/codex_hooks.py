from __future__ import annotations

from ci.claude_hooks import evaluate_bash_command


def pre_tool_use_response(payload: dict[str, object]) -> dict[str, object] | None:
    tool_name = payload.get("tool_name")
    if tool_name != "Bash":
        return None
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return None
    command = tool_input.get("command")
    if not isinstance(command, str):
        return None

    decision = evaluate_bash_command(command)
    if decision is None:
        return None

    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision.permission_decision,
            "permissionDecisionReason": decision.reason,
        }
    }
