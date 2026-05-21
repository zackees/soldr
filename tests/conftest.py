"""Pytest collection hooks shared across the soldr test suite."""

from __future__ import annotations

import pytest


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--act-integration",
        action="store_true",
        default=False,
        help=(
            "Run docker-based act-image integration tests (marked "
            "@pytest.mark.act_integration). Skipped by default because they "
            "require docker + ~1 GB image pulls."
        ),
    )


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    if config.getoption("--act-integration"):
        return
    selected_via_marker_expression = "act_integration" in (config.getoption("-m") or "")
    if selected_via_marker_expression:
        return
    skip_marker = pytest.mark.skip(
        reason=(
            "act_integration tests are opt-in. Re-run with --act-integration "
            "or `-m act_integration` to execute them."
        )
    )
    for item in items:
        if "act_integration" in item.keywords:
            item.add_marker(skip_marker)
