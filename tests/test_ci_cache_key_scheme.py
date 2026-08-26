"""Guards for the CI cache-key scheme (soldr#1978 item 6).

The scheme is "key on (profile, target), not on job": ordinary CI lanes that
compile the same dependency graph at the same profile for the same triple
share one `ws-<profile>-<target>` namespace instead of one namespace each.

Two things about it are load-bearing and easy to break by accident, so they
are pinned here rather than left to review:

1. **Performance lanes must stay off the scheme.** `perf-cold-warm.yml`
   deletes every repo cache whose key merely *contains* `perf-cold-warm`, and
   `parent-cache-bench.yml` builds and measures in the same job. Pulling
   either onto a shared namespace either breaks a documented cold guarantee
   or lets one workflow delete/contaminate another's cache.

2. **A shared key only pays if the sharers write the same directory.** `lint`
   and `build-linux-x64` both compile ~700 dependencies at the dev profile
   for x86_64-unknown-linux-gnu, but cargo splits `target/debug` from
   `target/<triple>/debug`, so they shared nothing until both passed
   `--target`. The key and the flag have to move together; a later edit
   dropping the flag would leave a shared key that is pure cosmetics.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"

# Workflows whose cache namespace must stay private to them.
PERF_WORKFLOWS = {
    "perf-matrix.yml",
    "perf-cold-warm.yml",
    "parent-cache-bench.yml",
    "cache-delta-experiment.yml",
}

SHARED_KEY = re.compile(r"^\s*shared-key:\s*(?P<key>\S.*?)\s*$", re.MULTILINE)


def shared_keys(name: str) -> list[str]:
    return [m.group("key") for m in SHARED_KEY.finditer(read(name))]


def read(name: str) -> str:
    return (WORKFLOWS / name).read_text(encoding="utf-8")


def test_perf_lanes_stay_off_the_shared_scheme() -> None:
    for name in PERF_WORKFLOWS:
        for key in shared_keys(name):
            assert not key.startswith("ws-"), (
                f"{name} uses shared cache key {key!r}; performance and "
                "cache-strategy lanes must keep a private namespace so an "
                "ordinary CI lane cannot alter the state they measure"
            )


def test_perf_cold_warm_key_stays_purgeable() -> None:
    """Its own purge job finds caches by substring; the key must match it."""
    text = read("perf-cold-warm.yml")
    suffix = re.search(r"^\s*DEMO_CACHE_SUFFIX:\s*(\S+)\s*$", text, re.MULTILINE)
    assert suffix, "perf-cold-warm.yml no longer defines DEMO_CACHE_SUFFIX"
    marker = suffix.group(1)
    assert 'contains(\\"${DEMO_CACHE_SUFFIX}\\")' in text, (
        "the purge job no longer selects caches by DEMO_CACHE_SUFFIX; "
        "re-check that the rust-cache key below is still reachable by it"
    )
    for key in shared_keys("perf-cold-warm.yml"):
        assert marker in key, (
            f"cache key {key!r} does not contain {marker!r}, so "
            "purge-demo-caches will no longer delete it and the documented "
            "cold-start guarantee silently stops holding"
        )


def test_no_ci_key_collides_with_the_perf_purge_substring() -> None:
    """The purge is a substring match, so it can reach outside its workflow."""
    text = read("perf-cold-warm.yml")
    suffix = re.search(r"^\s*DEMO_CACHE_SUFFIX:\s*(\S+)\s*$", text, re.MULTILINE)
    assert suffix
    marker = suffix.group(1)
    for path in sorted(WORKFLOWS.glob("*.yml")):
        if path.name == "perf-cold-warm.yml":
            continue
        for key in shared_keys(path.name):
            assert marker not in key, (
                f"{path.name} cache key {key!r} contains {marker!r}; a "
                "perf-cold-warm dispatch would delete this cache, because "
                "its purge job matches every repo cache key by substring"
            )


def test_native_lane_owns_the_shared_dev_namespace_and_target_dir() -> None:
    """The non-Rust lint lane must not claim or populate the Rust cache."""
    ci = read("ci.yml")
    build_and_test = read("_build-and-test.yml")

    assert "shared-key: ws-dev-x86_64-unknown-linux-gnu" not in ci
    assert build_and_test.count("shared-key: ws-dev-${{ inputs.target }}") == 1
    assert "--target ${{ inputs.target }}" in build_and_test, (
        "_build-and-test.yml lost its explicit host target, so the driver and "
        "ci-test could populate different target directories"
    )


def test_baseline_zero_deps_pair_shares_one_release_namespace() -> None:
    """`build-soldr` needs `bootstrap-soldr`, so one populates, one restores."""
    text = read("baseline-zero-deps.yml")
    keys = shared_keys("baseline-zero-deps.yml")
    assert "ws-release-x86_64-unknown-linux-gnu" in keys
    assert "ws-release-${{ matrix.target }}" in keys
    assert "needs: bootstrap-soldr" in text, (
        "the two release lanes no longer run in sequence, so sharing one "
        "namespace is no longer a populate-then-restore relationship"
    )
