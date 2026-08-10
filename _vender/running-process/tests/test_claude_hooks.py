from __future__ import annotations

from ci.claude_hooks import (
    MANDATE_REASON,
    UV_RUN_REASON,
    evaluate_bash_command,
    pre_tool_use_response,
)


def test_evaluate_bash_command_denies_direct_cargo() -> None:
    decision = evaluate_bash_command("cargo build --workspace")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == MANDATE_REASON


def test_evaluate_bash_command_ignores_direct_maturin() -> None:
    assert evaluate_bash_command("maturin build --release") is None


def test_evaluate_bash_command_allows_soldr_prefix() -> None:
    assert evaluate_bash_command("soldr cargo test --workspace") is None


def test_evaluate_bash_command_denies_compound_raw_build_command() -> None:
    decision = evaluate_bash_command("cargo build && cargo test")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == MANDATE_REASON


def test_evaluate_bash_command_denies_raw_rustc() -> None:
    decision = evaluate_bash_command("rustc --version")

    assert decision is not None
    assert decision.permission_decision == "deny"


def test_evaluate_bash_command_allows_repo_entrypoints() -> None:
    assert evaluate_bash_command("./test") is None
    assert evaluate_bash_command("./lint") is None
    assert evaluate_bash_command("uv run build.py") is None


def test_evaluate_bash_command_does_not_match_cargo_inside_other_args() -> None:
    assert evaluate_bash_command('gh pr create --body "cargo build --workspace"') is None


def test_uv_run_without_safe_flag_is_denied() -> None:
    decision = evaluate_bash_command("uv run pytest tests")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == UV_RUN_REASON


def test_uv_run_with_no_project_is_allowed() -> None:
    assert evaluate_bash_command("uv run --no-project --module ci.version_check") is None


def test_uv_run_with_no_sync_is_allowed() -> None:
    assert evaluate_bash_command("uv run --no-sync pytest tests") is None


def test_uv_run_with_frozen_is_allowed() -> None:
    assert evaluate_bash_command("uv run --frozen pytest") is None


def test_uv_run_module_without_safe_flag_is_denied() -> None:
    decision = evaluate_bash_command("uv run --module ci.version_check")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == UV_RUN_REASON


def test_uv_run_in_chained_segment_is_denied() -> None:
    decision = evaluate_bash_command("echo starting && uv run pytest tests")

    assert decision is not None
    assert decision.permission_decision == "deny"
    assert decision.reason == UV_RUN_REASON


def test_build_entrypoint_uv_run_is_still_allowed() -> None:
    # ALLOW_PREFIXES short-circuits before the safe-flag check fires,
    # preserving the legitimate full-rebuild paths.
    assert evaluate_bash_command("uv run build.py") is None
    assert evaluate_bash_command("uv run build.py --release") is None
    assert evaluate_bash_command("uv run --module ci.build_wheel --dev") is None


def test_uv_tool_run_is_not_caught() -> None:
    # `uv tool run` is a different subcommand that doesn't auto-sync the
    # project; the ban only targets `uv run`.
    assert evaluate_bash_command("uv tool run ruff check") is None


def test_pre_tool_use_response_denies_raw_cargo() -> None:
    response = pre_tool_use_response(
        {
            "tool_name": "Bash",
            "tool_input": {
                "command": "cargo build --workspace",
                "description": "build rust",
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
