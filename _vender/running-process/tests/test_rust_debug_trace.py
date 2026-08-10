from __future__ import annotations

import os

import pytest
from running_process._native import native_test_capture_rust_debug_trace

live = pytest.mark.live
skip_unless_github_actions = pytest.mark.skipif(
    os.environ.get("GITHUB_ACTIONS", "").lower() != "true",
    reason="requires GitHub Actions runner",
)
pytestmark = [live, skip_unless_github_actions]


def test_native_test_capture_rust_debug_trace_includes_nested_frames() -> None:
    trace = native_test_capture_rust_debug_trace()

    assert "running_process_py::native_test_capture_rust_debug_trace::outer" in trace
    assert "running_process_py::native_test_capture_rust_debug_trace::inner" in trace
