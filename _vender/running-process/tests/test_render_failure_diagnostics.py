from __future__ import annotations

from ci import render_failure_diagnostics as module


def test_analytics_failure_excerpt_prefers_explicit_excerpt() -> None:
    excerpt = module._analytics_failure_excerpt(
        {
            "pytest_failure_excerpt": [
                "=================================== FAILURES ===================================",
                "E       assert left == right",
            ],
            "tail_lines": ["later tail noise"],
        }
    )

    assert excerpt == [
        "=================================== FAILURES ===================================",
        "E       assert left == right",
    ]


def test_analytics_failure_excerpt_falls_back_to_tail_lines() -> None:
    excerpt = module._analytics_failure_excerpt(
        {
            "tail_lines": [
                "tests/test_cli.py::test_target_case FAILED [ 50%]",
                "=================================== FAILURES ===================================",
                "E       assert left == right",
            ]
        }
    )

    assert excerpt == [
        "=================================== FAILURES ===================================",
        "E       assert left == right",
    ]


def test_extract_pytest_failure_excerpt_uses_summary_section_context() -> None:
    excerpt = module._extract_pytest_failure_excerpt(
        [
            "tests/test_cli.py::test_target_case FAILED [ 50%]",
            "captured stdout",
            "=========================== short test summary info ===========================",
            "FAILED tests/test_cli.py::test_target_case - AssertionError: boom",
        ]
    )

    assert excerpt == [
        "tests/test_cli.py::test_target_case FAILED [ 50%]",
        "captured stdout",
        "=========================== short test summary info ===========================",
        "FAILED tests/test_cli.py::test_target_case - AssertionError: boom",
    ]
