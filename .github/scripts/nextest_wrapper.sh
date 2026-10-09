#!/bin/sh
# Nextest run-wrapper entry point (soldr#3453, soldr#3454).
#
# Every Unix test runs through the native `soldr-nextest-wrapper`
# (crates/soldr-nextest-wrapper), the one implementation of the wrapper
# contract. This shim only finds it, by one rule:
#
#   1. $SOLDR_NEXTEST_NATIVE_WRAPPER, when set (an explicit override; the
#      black-box suites use their own SOLDR_NEXTEST_WRAPPER_UNDER_TEST);
#   2. otherwise beside the test binary's Cargo profile directory:
#      `<target>/[<triple>/]<profile>/deps/<test>` ->
#      `<target>/[<triple>/]<profile>/soldr-nextest-wrapper`. Cargo builds the
#      wrapper there whenever its package is selected (it has integration
#      tests, so its bin is always built with them), and `nextest archive`
#      ships it at the same relative path as a non-test binary, so a
#      `--workspace` build, a Nextest archive extracted on a target host
#      (target-run lanes, the macOS Recovery replay) and `soldr ci-test` all
#      resolve it the same way. The first argument under a `deps/` directory
#      is the test binary even when a target runner precedes it.
#   3. otherwise refuse, loudly: a test never runs unwrapped.
#
# POSIX parameter expansion only -- no dirname, no python3: the macOS
# Recovery guest has neither guaranteed.
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
for arg in "$@"; do
    parent="${arg%/*}"
    case "$parent" in
    */deps)
        if [ -x "${parent%/*}/soldr-nextest-wrapper" ]; then
            exec "${parent%/*}/soldr-nextest-wrapper" "$@"
        fi
        ;;
    esac
done
echo "nextest_wrapper.sh: soldr-nextest-wrapper not found (soldr#3454)." >&2
echo "  looked beside the test binary's profile directory (<profile>/soldr-nextest-wrapper) for: $*" >&2
echo "  build it with the tests (select the soldr-nextest-wrapper package, e.g. --workspace," >&2
echo "  or: soldr cargo build -p soldr-nextest-wrapper with the same --target/--profile)," >&2
echo "  or name it in SOLDR_NEXTEST_NATIVE_WRAPPER. Tests never run unwrapped." >&2
exit 98
