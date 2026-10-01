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
#
# zackees/ci.yml#168 (GATE-005), soldr#3516: soldr's tests start soldr
# daemons and touch soldr state roots, so they never run on a developer host
# -- a leaked fixture daemon once claimed the real ~/.soldr root and wedged
# every soldr build on the machine. Every Unix test process passes through
# here, so this is where the suite refuses. CI=true covers GitHub runners and
# act; SOLDR_TEST_ISOLATED=1 is set only by isolated environments (the bosn
# image, the macOS Recovery guest).
if [ "${CI:-}" != "true" ] && [ "${SOLDR_TEST_ISOLATED:-}" != "1" ]; then
    echo "soldr tests refuse to run on a developer host (zackees/ci.yml#168, soldr#3516)." >&2
    echo "Run them isolated: bosn run --task test" >&2
    exit 97
fi
if [ -n "${SOLDR_NEXTEST_NATIVE_WRAPPER:-}" ]; then
    exec "$SOLDR_NEXTEST_NATIVE_WRAPPER" "$@"
fi
exec "$(dirname "$0")/nextest_timeout_wrapper.py" "$@"
