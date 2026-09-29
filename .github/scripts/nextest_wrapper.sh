#!/bin/sh
# Nextest run-wrapper entry point (soldr#3453).
#
# `soldr ci-test` names the native `soldr-nextest-wrapper` binary in
# SOLDR_NEXTEST_NATIVE_WRAPPER for its Linux Nextest execution stage; it
# replaces the Python wrapper's ~48 ms interpreter start before every test.
# Everything else -- local `soldr cargo nextest run`, archived target runs,
# macOS -- keeps the Python wrapper. Both implementations pass the same
# black-box suites (tests/test_nextest_timeout_wrapper.py and
# tests/test_nextest_memory_guard.py with SOLDR_NEXTEST_WRAPPER_UNDER_TEST).
if [ -n "${SOLDR_NEXTEST_NATIVE_WRAPPER:-}" ]; then
    exec "$SOLDR_NEXTEST_NATIVE_WRAPPER" "$@"
fi
exec "$(dirname "$0")/nextest_timeout_wrapper.py" "$@"
