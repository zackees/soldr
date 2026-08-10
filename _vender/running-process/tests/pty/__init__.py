"""Tests for PseudoTerminalProcess and related PTY support.

This package was split out of the former monolithic ``tests/test_pty_support.py``.
Each module focuses on a single feature area so future agents can read just the
relevant test file. Shared helpers and autouse fixtures live in
``tests/pty/_pty_helpers.py``.
"""
