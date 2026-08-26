from __future__ import annotations

import os
import signal
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
WRAPPER = REPO_ROOT / ".github" / "scripts" / "nextest_timeout_wrapper.py"
CONFIG = REPO_ROOT / ".config" / "nextest.toml"


def _start_wrapper(child: str, env: dict[str, str]) -> subprocess.Popen[str]:
    """Return a live wrapper so the test can inject SIGTERM before waiting."""

    return subprocess.Popen(  # pylint: disable=consider-using-with
        [sys.executable, str(WRAPPER), sys.executable, "-c", child],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_sigterm_dumps_threads_and_drains_child_output() -> None:
    child = """
import signal
import threading
import time

def stop(_signum, _frame):
    print("child output after termination", flush=True)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
threading.Thread(target=lambda: time.sleep(60), daemon=True).start()
print("child output before timeout", flush=True)
while True:
    time.sleep(0.1)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    env["SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS"] = "0.5"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    before_timeout = process.stdout.readline()
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=15)
    stdout = before_timeout + stdout_tail

    assert process.returncode == 0
    assert "child output before timeout" in stdout
    assert "child output after termination" in stdout
    assert "nextest timeout: thread dump for pid" in stderr
    assert "nextest timeout: thread dump complete" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr
    if sys.platform.startswith("linux"):
        assert stderr.count("--- thread") >= 2
    else:
        assert "no platform thread dumper is available" in stderr


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_timeout_kills_descendant_that_retains_output_pipes() -> None:
    child = """
import signal
import subprocess
import sys
import time

grandchild = '''
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print("grandchild inherited output pipes", flush=True)
while True:
    time.sleep(1)
'''
subprocess.Popen([sys.executable, "-c", grandchild])

def stop(_signum, _frame):
    print("direct child exited after termination", flush=True)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
print("direct child ready", flush=True)
while True:
    time.sleep(0.1)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    ready_lines = [process.stdout.readline(), process.stdout.readline()]
    assert any("direct child ready" in line for line in ready_lines)
    assert any("grandchild inherited output pipes" in line for line in ready_lines)
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=15)
    stdout = "".join(ready_lines) + stdout_tail

    assert process.returncode == 0
    assert "direct child ready" in stdout
    assert "direct child exited after termination" in stdout
    assert "grandchild inherited output pipes" in stdout
    assert "descendants retained output pipes; forcing exit" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr


@pytest.mark.skipif(
    os.name != "posix", reason="Nextest has no Windows timeout grace hook"
)
def test_signal_during_drain_kills_descendant_after_leader_already_exited() -> None:
    child = """
import signal
import subprocess
import sys

grandchild = '''
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print("orphan grandchild retained output pipes", flush=True)
while True:
    time.sleep(1)
'''
subprocess.Popen([sys.executable, "-c", grandchild])
print("direct child exiting normally", flush=True)
"""
    env = os.environ.copy()
    env["SOLDR_NEXTEST_DISABLE_DEBUGGER"] = "1"
    env["SOLDR_NEXTEST_CHILD_EXIT_GRACE_SECS"] = "0.5"
    process = _start_wrapper(child, env)
    assert process.stdout is not None
    ready_lines = [process.stdout.readline(), process.stdout.readline()]
    assert any("direct child exiting normally" in line for line in ready_lines)
    assert any(
        "orphan grandchild retained output pipes" in line for line in ready_lines
    )
    process.send_signal(signal.SIGTERM)
    stdout_tail, stderr = process.communicate(timeout=10)
    stdout = "".join(ready_lines) + stdout_tail

    assert process.returncode == 0
    assert "direct child exiting normally" in stdout
    assert "orphan grandchild retained output pipes" in stdout
    assert "descendants retained output pipes; forcing exit" in stderr
    assert "nextest timeout: stdout/stderr drained" in stderr


def test_nextest_config_wraps_unix_tests_with_a_bounded_grace_period() -> None:
    config = CONFIG.read_text(encoding="utf-8")
    assert 'experimental = ["wrapper-scripts"]' in config
    assert 'run-wrapper = "timeout-diagnostics"' in config
    assert 'platform = { target = "cfg(unix)" }' in config
    assert 'platform = { host = "cfg(unix)" }' not in config
    assert "[test-groups.soldr-runtime]" in config
    assert "binary(cli_broker_resurrection) + binary(cli_broker_routes)" in config
    # Three override blocks, one group. The count is the point: broker
    # cold-start tests must land in the SAME group, because separate
    # one-thread groups still run concurrently and contend for the bounded
    # route-start path (soldr#2336).
    assert config.count("test-group = 'soldr-runtime'") == 3
    assert "test(=gc_list_json_reports_built_project_target_dir)" in config
    runtime_overrides = [
        block
        for block in config.split("[[profile.default.overrides]]")[1:]
        if "test-group = 'soldr-runtime'" in block
    ]
    assert len(runtime_overrides) == 3
    runtime_members = "".join(runtime_overrides)
    for cold_start_test in (
        "test(=toolchain_link_writes_every_routed_tool_into_shim_dir)",
        "test(=toolchain_link_is_idempotent_when_rerun_with_same_soldr_binary)",
        # soldr#2729: the other two members of the same binary, measured
        # timing out on `main` itself once the first pair was treated.
        "test(=toolchain_link_emits_schema_v1_json_payload)",
        "test(=toolchain_link_force_overwrites_user_modified_shim)",
        "test(=cargo_fmt_host_toolchain_does_not_mix_in_managed_rustup_home)",
    ):
        assert cold_start_test in runtime_members
    # soldr#2729: group membership alone is not the whole treatment. The four
    # `toolchain link` tests also need the 240s budget their measured siblings
    # got -- a test that only checked the group would pass while two of them
    # kept dying at the 120s default.
    budget_overrides = [
        block
        for block in config.split("[[profile.default.overrides]]")[1:]
        # Match the filter line, not the block text: splitting on the
        # override marker leaves each block carrying the *next* one's
        # leading comment, which mentions toolchain_link too.
        if "terminate-after = 4" in block
        and "filter = 'test(=toolchain_link_writes" in block
    ]
    assert len(budget_overrides) == 1
    for linked in (
        "test(=toolchain_link_writes_every_routed_tool_into_shim_dir)",
        "test(=toolchain_link_is_idempotent_when_rerun_with_same_soldr_binary)",
        "test(=toolchain_link_emits_schema_v1_json_payload)",
        "test(=toolchain_link_force_overwrites_user_modified_shim)",
    ):
        assert linked in budget_overrides[0]

    assert "[test-groups.soldr-cargo-cold-builds]" in config
    cold_overrides = [
        block
        for block in config.split("[[profile.default.overrides]]")[1:]
        if "test-group = 'soldr-cargo-cold-builds'" in block
    ]
    assert len(cold_overrides) == 1
    cold_override = cold_overrides[0]
    # Per member, not as one contiguous literal. The literal also pinned the
    # order and pinned that nothing sat between the terms, so soldr#2887 broke
    # this test by *adding* `binary(cli_dylint_wrapper)` in the middle -- a
    # legitimate edit failing a guard that was only meant to check membership.
    # What the reservation cares about is which members are in it, which the
    # loop below states and the exclusions further down still pin.
    for cold_binary in (
        "binary(cli_cargo_basic)",
        "binary(cli_cargo_linker)",
        "binary(cli_cargo_run_trampoline)",
        "binary(cli_cargo_wrappers)",
        "binary(cli_dylint_wrapper)",
        "test(=cargo_front_door_invokes_zccache_rust_plan_when_target_cache_enabled)",
    ):
        assert cold_binary in cold_override, cold_override
    # soldr#2737: `cli_cargo_basic` is binary-scoped because 19 of its 21
    # tests drive `isolated_soldr_command`. Its per-test entry is subsumed and
    # must not come back alongside the binary one -- two entries covering the
    # same tests is how the list stopped being readable.
    assert (
        "test(=cargo_without_timeout_allows_progress_cpu_and_lock_waits)"
        not in cold_override
    )
    # These stay per-test on purpose: they live in binaries that are not
    # predominantly cold front doors (soldr#2697, soldr#2720).
    for cold_member in (
        "test(=cargo_front_door_forces_msvc_target_even_with_polluted_path)",
        "test(=exec_cargo_build_routes_through_child_shims_and_zccache)",
    ):
        assert cold_member in cold_override
    assert 'threads-required = "num-cpus"' in cold_override
    assert "binary(cli_build_alias_parity)" in config
    assert "binary(cli_build_fetch_overlap)" in config
    for binary in (
        "agent_worktree_share",
        "cli_daemon_builds",
        "cli_daemon_flush_caches",
        "cli_daemon_lifecycle",
        "cli_daemon_single_instance",
        "cli_daemon_target_touch",
        "daemon_cache_maintenance",
        "daemon_stall_harness",
    ):
        assert f"binary({binary})" in config
    # nextest hard-validates binary names in filter expressions, so a filter
    # naming a deleted test binary fails EVERY `nextest run` at config-parse
    # time (exit 96) -- this is how the whole CI matrix went red when
    # cli_daemon_tombstone and session_multiprocess_smoke were removed
    # without updating the config (soldr#2553 fallout).
    for deleted in ("cli_daemon_tombstone", "session_multiprocess_smoke"):
        assert f"binary({deleted})" not in config
    assert config.count('threads-required = "num-cpus"') == 2
    # Every raised budget must carry the explicit grace period.
    #
    # The count below used to be the whole check, and its comment claimed it
    # "stops one being added without one" -- which it never did. A block added
    # *with* no grace period leaves the count unchanged and passes; only
    # adding one and forgetting to bump the number failed, i.e. exactly
    # backwards from the stated intent. Verified by adding a bare
    # `terminate-after` block: the old assertion did not notice.
    for block in config.split("[[profile.default.overrides]]")[1:]:
        if "terminate-after" not in block:
            continue
        assert (
            'grace-period = "30s"' in block
        ), f"a raised budget with no explicit grace period:\n{block}"
    # Kept beside it: the count still makes an addition deliberate rather than
    # incidental. 9 = the default profile + eight measured per-test override
    # blocks. Newest: soldr#2887's `binary(cli_dylint_wrapper)` block, whose
    # two fake-dylint front doors the prescribed `soldr ci-test` run measured
    # at 121s each when they raced.
    assert config.count('grace-period = "30s"') == 9


def test_every_binary_named_in_nextest_filters_is_a_real_test_target() -> None:
    """Deleting a test file without updating nextest.toml breaks ALL lanes.

    Generalizes the soldr#2553 lesson beyond the two names it burned us with:
    every `binary(NAME)` in a filter must resolve to `crates/*/tests/NAME.rs`
    (or a `[[bench]]`/example target), so the mismatch fails here in the fast
    Lint job with the offending name, not as exit-96 in every nextest lane.
    """
    import re

    config = CONFIG.read_text(encoding="utf-8")
    named = set(re.findall(r"binary\(([A-Za-z0-9_-]+)\)", config))
    assert named, "expected at least one binary() filter in nextest.toml"
    test_targets = {path.stem for path in REPO_ROOT.glob("crates/*/tests/*.rs")} | {
        path.stem for path in REPO_ROOT.glob("crates/*/benches/*.rs")
    }
    missing = sorted(named - test_targets)
    assert not missing, (
        f"nextest.toml filters name test binaries that do not exist: {missing}; "
        "nextest refuses to parse the config for EVERY test run when a filter "
        "names a missing binary -- update .config/nextest.toml in the same "
        "commit that deletes or renames a test file"
    )


def _terminate_after_budget_secs(config: str, test_name: str) -> int:
    """Seconds nextest allows `test_name` before it kills the process.

    nextest's bound is `period x terminate-after`; the grace period is what it
    waits after SIGTERM, not extra runtime. Blocks are scanned in file order and
    the last match wins, matching nextest's own override precedence.
    """
    import re

    budget: int | None = None
    for block in config.split("[[profile.default.overrides]]")[1:]:
        if f"test(={test_name})" not in block:
            continue
        match = re.search(
            r'slow-timeout\s*=\s*\{[^}]*period\s*=\s*"(\d+)s"[^}]*'
            r"terminate-after\s*=\s*(\d+)",
            block,
        )
        if match:
            budget = int(match.group(1)) * int(match.group(2))
    assert budget is not None, f"no terminate-after budget for {test_name}"
    return budget


def test_the_cache_maintenance_fixture_deadline_fits_two_cold_daemon_starts() -> None:
    """soldr#2883, second instance: a fixture deadline below its real work.

    `prod_dev_daemons_and_manual_orphan_maintenance_are_isolated` spawns two
    daemons, then bounds four sequential waits -- two readiness polls and two
    maintenance-status appearances -- with one deadline. At 60s the darwin x64
    lane exhausted it during startup and failed at 62.8s, with the other 2840
    tests in that run passing.

    Two bounds, and only one of them was wrong. The deadline must:

    * clear two concurrent cold embedded-service initializations with real
      headroom -- 60s did not, which is the bug; and
    * stay under the budget nextest grants the test, so the fixture's domain
      message (`daemon did not become ready`) is what a reader sees rather than
      a generic kill.

    The second clause held at 60s, which is why the config alone looked fine.
    Only reading the fixture literal against the config shows the first.
    """
    import re

    test_name = "prod_dev_daemons_and_manual_orphan_maintenance_are_isolated"
    fixture = (
        REPO_ROOT / "crates" / "soldr-cli" / "tests" / "daemon_cache_maintenance.rs"
    )
    source = fixture.read_text(encoding="utf-8")
    body = source.split(f"fn {test_name}", 1)
    assert len(body) == 2, f"{fixture.name} no longer defines {test_name}"

    deadlines = [
        int(secs)
        for secs in re.findall(
            r"let deadline = Instant::now\(\) \+ Duration::from_secs\((\d+)\)",
            body[1],
        )
    ]
    assert deadlines, "expected the readiness phase to carry an explicit deadline"

    budget = _terminate_after_budget_secs(CONFIG.read_text(encoding="utf-8"), test_name)
    for deadline in deadlines:
        assert deadline >= 120, (
            f"{test_name} allows {deadline}s for two cold daemon starts plus two "
            "maintenance writes; the darwin lane needed more than 60s of real "
            "work, so this reproduces soldr#2883"
        )
        assert deadline < budget, (
            f"{test_name} bounds itself at {deadline}s against a {budget}s nextest "
            "budget; past it the generic kill lands first and the fixture's own "
            "diagnosis is lost"
        )
