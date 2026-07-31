from __future__ import annotations

import json
import shlex
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
GUARDRAILS_PATH = REPO_ROOT / "contracts" / "zccache-integration-guardrails.v1.json"
DOCS_PATH = REPO_ROOT / "docs" / "ZCCACHE_INTEGRATION_GUARDRAILS.md"

REQUIRED_GUARDRAIL_IDS = {
    "embedded-runtime-topology",
    "embedded-session-env",
    "rust-plan-cache",
    "disabled-and-non-build",
    "embedded-flush-shutdown",
    "setup-action-outputs",
    "release-npm-staging",
    "perf-cold-warm",
    "perf-worktree-share",
    "perf-touch-no-change",
    "perf-build-then-check",
    "native-cc-cache",
    "monolith-migration-ratchet",
}

REQUIRED_COMMAND_IDS = {
    "rust-embedded-runtime-topology",
    "rust-zccache-entry",
    "rust-wrapper-env",
    "rust-rust-plan",
    "rust-cache-cli",
    "rust-cache-session",
    "rust-native-cache-unit",
    "rust-native-cache-integration",
    "rust-wrapper-perf",
    "rust-watchdog-lint",
    "python-setup-contracts",
    "node-npm-contract",
    "perf-cold-warm",
    "perf-matrix-worktree-touch",
}


def _guardrails() -> dict[str, Any]:
    return json.loads(GUARDRAILS_PATH.read_text(encoding="utf-8"))


def test_guardrail_contract_has_required_axes_and_commands() -> None:
    contract = _guardrails()

    assert contract["schema_version"] == 1
    assert contract["parent_issue"] == "https://github.com/zackees/soldr/issues/543"
    assert contract["wave_issue"] == "https://github.com/zackees/soldr/issues/548"

    commands = contract["validation_commands"]
    command_ids = {command["id"] for command in commands}
    assert len(commands) == len(command_ids), "validation command IDs must be unique"
    assert command_ids == REQUIRED_COMMAND_IDS
    for command in commands:
        assert command["gate"] in {"hard", "report-only"}
        assert command["command"].strip()

    guardrails = contract["guardrails"]
    guardrail_ids = {guardrail["id"] for guardrail in guardrails}
    assert len(guardrails) == len(guardrail_ids), "guardrail IDs must be unique"
    assert guardrail_ids == REQUIRED_GUARDRAIL_IDS
    referenced_command_ids: set[str] = set()
    for guardrail in guardrails:
        assert guardrail["gate"] in {"hard", "report-only"}
        assert guardrail["axis"]
        assert guardrail["covers"]
        assert guardrail["validation_command_ids"]
        assert set(guardrail["validation_command_ids"]).issubset(command_ids)
        referenced_command_ids.update(guardrail["validation_command_ids"])
    assert referenced_command_ids == command_ids


def test_guardrail_test_files_exist() -> None:
    contract = _guardrails()

    for guardrail in contract["guardrails"]:
        for rel_path in guardrail["test_files"]:
            path = REPO_ROOT / rel_path
            assert (
                path.exists()
            ), f"{guardrail['id']} references missing path: {rel_path}"


def test_rust_validation_command_targets_exist() -> None:
    contract = _guardrails()

    for validation in contract["validation_commands"]:
        command = validation["command"]
        if " cargo test " not in command:
            continue

        tokens = shlex.split(command)
        if "--test" in tokens:
            index = tokens.index("--test")
            assert index + 1 < len(tokens), f"{validation['id']} has a bare --test flag"
            target = tokens[index + 1]
            path = REPO_ROOT / "crates" / "soldr-cli" / "tests" / f"{target}.rs"
            assert path.is_file(), (
                f"{validation['id']} references missing cargo test target: {target} "
                f"({path.relative_to(REPO_ROOT)})"
            )

        if "--lib" in tokens:
            lib_path = REPO_ROOT / "crates" / "soldr-cli" / "src" / "lib.rs"
            assert lib_path.is_file(), (
                f"{validation['id']} requires missing cargo lib target: "
                f"{lib_path.relative_to(REPO_ROOT)}"
            )


def test_guardrail_docs_cover_every_id_and_command() -> None:
    contract = _guardrails()
    docs = DOCS_PATH.read_text(encoding="utf-8")

    assert "contracts/zccache-integration-guardrails.v1.json" in docs
    assert "issue #543" in docs
    for guardrail in contract["guardrails"]:
        assert f"`{guardrail['id']}`" in docs
    for command in contract["validation_commands"]:
        assert command["command"] in docs


def test_npm_package_includes_guardrail_contract() -> None:
    package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))

    assert "contracts/zccache-runtime.v1.json" in package["files"]
    assert "contracts/zccache-integration-guardrails.v1.json" in package["files"]
