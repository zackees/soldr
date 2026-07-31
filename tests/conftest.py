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
    parser.addoption(
        "--cacheability-integration",
        action="store_true",
        default=False,
        help=(
            "Run Docker-based cacheability integration tests (marked "
            "@pytest.mark.cacheability_integration). Skipped by default "
            "because they build the full nextest archive twice."
        ),
    )


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    marker_options = {
        "act_integration": "--act-integration",
        "cacheability_integration": "--cacheability-integration",
    }
    marker_expression = config.getoption("-m") or ""
    for item in items:
        for marker, option in marker_options.items():
            if marker not in item.keywords:
                continue
            if config.getoption(option) or marker in marker_expression:
                continue
            item.add_marker(
                pytest.mark.skip(
                    reason=(
                        f"{marker} tests are opt-in. Re-run with {option} "
                        f"or `-m {marker}` to execute them."
                    )
                )
            )
