"""Regression tests for the two assertion guards in `_build-and-test.yml`.

Both steps assert something that never fails loudly on its own:

* soldr#1838 -- the build silently ran uncached (a daemon fallback);
* soldr#1799 -- soldr's managed toolchain homes leaked onto a host-resolved
  tool, which flips which rustc runs, invalidates cargo fingerprints and
  zccache keys, and leaves warm builds recompiling the world.

They are deliberately gated differently, and the difference is easy to erase
by copying one line from the other:

* #1838 is advisory off `linux-gnu`, because a flaky Windows/macOS daemon
  race should surface in the log without gating the build. That is a stated
  platform story.
* #1799 is unconditional, because `home_origin` correctness is the CLAUDE.md
  invariant on every platform. It carried the neighbour's advisory
  expression until soldr#1799 follow-up work removed it.

Neither expression is live today -- `ci.yml` is the only caller and passes
`x86_64-unknown-linux-gnu` -- so nothing at runtime would notice them
drifting. Hence a test.

Plain-text parsing, matching `test_thin_v2_verify_workflow.py`: pyyaml is not
a dependency here, and a guard that skips when a module is missing is the
failure mode these very steps exist to prevent.
"""

import re
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "_build-and-test.yml"
CARGO_TOML = REPO_ROOT / "Cargo.toml"
NEXTEST_CONFIG = REPO_ROOT / ".config" / "nextest.toml"

GUARD_1799 = "Assert managed toolchain homes did not leak (soldr#1799)"
GUARD_1838 = "Assert the build did not silently run uncached (soldr#1838)"
CI_TEST_DRIVER = "Build ci-test driver"
CI_TEST_RUN = "Run prescribed host validation"
BROKER_HANDOFF = "Hand off bootstrap broker to source revision"
DYLINT_TESTS_COOK = "Cook the Dylint UI-test dependency layer (soldr#3042)"
SOURCE_DRIVER_DOWNLOAD = "Reuse shared source driver if already available"
SOURCE_DRIVER_VERIFY = "Verify shared source driver provenance"
CANONICAL_CACHE = "Select canonical host CI cache domain"
FINAL_CACHE_STOP = "Stop canonical host CI cache"
CACHE_ENV_VARS = ("SOLDR_CACHE_DIR", "ZCCACHE_CACHE_DIR")
CACHE_SHELL_ASSIGNMENT = re.compile(
    r"^(?:(?:export|env)\s+)?(?:SOLDR_CACHE_DIR|ZCCACHE_CACHE_DIR)\s*="
    r"|^\$env:(?:SOLDR_CACHE_DIR|ZCCACHE_CACHE_DIR)\s*="
)


def _step_body(workflow: str, step_name: str) -> str:
    """The lines of one step, from its `- name:` to the next step's."""
    start = workflow.index(f"- name: {step_name}")
    nxt = workflow.find("\n      - name: ", start + 1)
    return workflow[start : nxt if nxt != -1 else len(workflow)]


def test_the_toolchain_home_guard_is_unconditional() -> None:
    # soldr#1799 acceptance: "CI workflow fails on any host-resolved tool
    # executing under managed homes" -- on any platform, not just Linux.
    body = _step_body(WORKFLOW.read_text(encoding="utf-8"), GUARD_1799)
    assert "continue-on-error" not in body, (
        "the #1799 toolchain-home guard must hard-fail everywhere; an "
        "advisory gate would let a home leak land silently, which is the "
        "entire failure mode the issue exists for"
    )


def test_the_uncached_build_guard_keeps_its_documented_platform_split() -> None:
    # The inverse: #1838 IS advisory off linux-gnu on purpose, and flipping it
    # to unconditional would gate builds on a known-flaky off-Linux race.
    body = _step_body(WORKFLOW.read_text(encoding="utf-8"), GUARD_1838)
    assert "continue-on-error" in body, (
        "the #1838 guard is intentionally advisory off linux-gnu; making it "
        "unconditional would gate the build on a flaky daemon race"
    )
    assert "linux-gnu" in body


def test_both_guards_still_exist() -> None:
    # Cheap protection against the step-body lookup silently matching nothing
    # if a step is renamed -- these assertions are only meaningful while the
    # steps they name are present.
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert workflow.count(f"- name: {GUARD_1799}") == 1
    assert workflow.count(f"- name: {GUARD_1838}") == 1


def test_the_guards_run_the_scripts_they_claim_to() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "check_toolchain_homes.py" in _step_body(workflow, GUARD_1799)
    assert "check_compile_fallbacks.py" in _step_body(workflow, GUARD_1838)


def test_hosted_runner_compile_concurrency_uses_heavy_unit_exclusivity() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    assert "CARGO_BUILD_JOBS:" not in workflow
    assert "SOLDR_JOBS:" not in workflow
    assert "exclusive compiler admission" in workflow
    assert "An OOM is an admission bug" in workflow
    assert "Enlarge swap (OOM headroom)" in workflow


def test_setup_soldr_job_caps_are_cleared_before_source_work() -> None:
    """The bootstrap action must not silently cap the source revision.

    setup-soldr 0.9.10 exports both legacy one-job overrides through
    ``GITHUB_ENV`` when ``ci-tests: true``.  Omitting the variables from the
    reusable job's ``env`` block therefore does not make them unset.  Keep the
    cleanup local to this host-validation boundary so normal callers' explicit
    Soldr/Cargo overrides continue to win everywhere else.
    """
    workflow = WORKFLOW.read_text(encoding="utf-8")
    clear_caps = "unset CARGO_BUILD_JOBS SOLDR_JOBS"

    command_by_step = {
        CI_TEST_DRIVER: "soldr cargo build",
        BROKER_HANDOFF: '"$source_soldr" daemon start',
        DYLINT_TESTS_COOK: "cook_dylint_tests_tree.py",
        CI_TEST_RUN: "ci-test --target",
    }
    for step_name, source_command in command_by_step.items():
        body = _step_body(workflow, step_name)
        assert clear_caps in body
        assert body.index(clear_caps) < body.index(source_command)


def test_dylint_tests_tree_is_cooked_between_broker_handoff_and_validation() -> None:
    """The tests-tree cook (soldr#3042) has a load-bearing position.

    It must run AFTER the broker handoff, not before: `--tree` exists only on
    the source binary (the pinned setup-soldr 0.9.10 builder has no such
    flag), and the compiles must route through the same source-owned daemon
    `ci-test` uses -- the handoff step is what makes that daemon current. It
    must run BEFORE prescribed host validation, or the whole point (keeping
    these compiles out of the concurrent Dylint UI-test / Fresh Nextest
    window) is lost. A future re-order that moves this step either direction
    silently reintroduces the contention soldr#3042 removed.
    """
    workflow = WORKFLOW.read_text(encoding="utf-8")

    assert workflow.count(f"- name: {DYLINT_TESTS_COOK}") == 1
    assert (
        workflow.index(f"- name: {BROKER_HANDOFF}")
        < workflow.index(f"- name: {DYLINT_TESTS_COOK}")
        < workflow.index(f"- name: {CI_TEST_RUN}")
    )

    body = _step_body(workflow, DYLINT_TESTS_COOK)
    assert "--target-root" in body
    assert "cook_dylint_tests_tree.py" in body
    assert "continue-on-error: true" not in body


def test_source_driver_reuse_is_exact_sha_opportunistic_and_fails_closed() -> None:
    """A same-run driver may save the duplicate link, never gate this lane."""
    workflow = WORKFLOW.read_text(encoding="utf-8")
    download = _step_body(workflow, SOURCE_DRIVER_DOWNLOAD)
    verify = _step_body(workflow, SOURCE_DRIVER_VERIFY)
    build = _step_body(workflow, CI_TEST_DRIVER)

    assert "source_driver_artifact_name:" in workflow
    assert "required: false" in workflow
    assert 'default: ""' in workflow

    assert "actions/download-artifact@" in download
    assert "continue-on-error: true" in download
    assert "inputs.source_driver_artifact_name != ''" in download
    assert "run-id:" not in download
    assert "repository:" not in download
    assert "github-token:" not in download

    assert "steps.source_driver_download.outcome == 'success'" in verify
    assert "continue-on-error: true" in verify
    assert 'expected_sha="${{ github.sha }}"' in verify
    assert 'actual_sha=$(<"$artifact_dir/source-sha")' in verify
    assert '[[ "$actual_sha" == "$expected_sha" ]]' in verify
    assert verify.index('[[ "$actual_sha" == "$expected_sha" ]]') < verify.index(
        '"$artifact_soldr" --version'
    )
    assert verify.index('"$artifact_soldr" --version') < verify.index(
        'cp "$artifact_soldr" "$source_soldr"'
    )

    assert "steps.shared_source_driver.outcome != 'success'" in build
    assert "soldr cargo build" in build
    assert 'source_soldr="${GITHUB_WORKSPACE}/target/' in verify


def test_source_compile_guards_cover_the_complete_validation_run() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    validation = workflow.index(f"- name: {CI_TEST_RUN}")
    fallback_guard = workflow.index(f"- name: {GUARD_1838}")
    home_guard = workflow.index(f"- name: {GUARD_1799}")

    assert validation < fallback_guard < home_guard
    assert "if: always()" in _step_body(workflow, GUARD_1838)
    assert "if: always()" in _step_body(workflow, GUARD_1799)


def test_dev_and_test_profiles_share_all_host_ci_compile_settings() -> None:
    # The warm-up path drives dev and nextest drives test. They share
    # target/<triple>/debug, so a setting delta makes every unit stale at the
    # handoff. Keep all dev overrides mirrored in test unless that handoff is
    # deliberately removed from the workflow.
    manifest = tomllib.loads(CARGO_TOML.read_text(encoding="utf-8"))
    profiles = manifest["profile"]
    assert profiles["test"] == profiles["dev"]


def test_ci_test_is_the_only_host_test_orchestration_entrypoint() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    body = _step_body(workflow, CI_TEST_RUN)
    assert workflow.count(f"- name: {CI_TEST_RUN}") == 1
    # soldr#2996 Phase 8: 2 after measurement (3 runs vs 6 baseline runs,
    # 4.0 min median saving, zero TIMEOUT lines). Pinned exactly rather
    # than as a range -- the resource contract is a deliberate number, so
    # the next change to it should be visible in review.
    assert 'NEXTEST_TEST_THREADS: "4"' in workflow
    assert 'SOLDR_RUSTC_WRAPPER="$source_soldr" "$source_soldr"' in body
    assert "bootstrap_wrapper" not in body
    assert workflow.count("- name: Hand off bootstrap broker to source revision") == 1
    assert "soldr broker remove" in workflow
    assert '"$source_soldr" daemon start' in workflow
    assert '"$source_soldr" broker remove' in workflow
    assert 'ci-test --target "${{ inputs.target }}"' in body
    assert "nextest run --no-run" not in workflow
    assert "Run library + CLI smoke tests" not in workflow


def test_ci_test_traps_bare_cargo_but_keeps_nested_cargo_on_soldr() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    body = _step_body(workflow, CI_TEST_RUN)

    assert 'real_cargo=$("$source_soldr" rustup which cargo)' in body
    assert "install_ci_cargo_guard.py" in body
    assert 'export SOLDR_REAL_CARGO="$real_cargo"' in body
    assert 'export CARGO="$(<"$cargo_guard/allowed-cargo-path")"' in body
    assert (
        'export SOLDR_CI_TEST_CARGO_RUNNER="$(<"$cargo_guard/test-runner-path")"'
        in body
    )
    assert 'export PATH="$cargo_guard/trap:$PATH"' in body
    assert body.index("install_ci_cargo_guard.py") < body.index(
        'SOLDR_RUSTC_WRAPPER="$source_soldr"'
    )
    assert "GITHUB_ENV" not in body
    assert "GITHUB_PATH" not in body
    assert "CARGO_BUILD_JOBS=" not in body
    assert "SOLDR_JOBS=" not in body

    nextest_config = NEXTEST_CONFIG.read_text(encoding="utf-8")
    wrapper_start = nextest_config.index("[scripts.wrapper.timeout-diagnostics]")
    wrapper_end = nextest_config.find("\n[", wrapper_start + 1)
    wrapper = nextest_config[
        wrapper_start : wrapper_end if wrapper_end != -1 else len(nextest_config)
    ]
    assert 'target-runner = "within-wrapper"' in wrapper


def test_canonical_cache_domain_precedes_every_host_build_and_test() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    domain = _step_body(workflow, CANONICAL_CACHE)
    assert workflow.count(f"- name: {CANONICAL_CACHE}") == 1
    setup_end = workflow.find(
        "\n      - name: ", workflow.index("Setup pinned soldr toolchain") + 1
    )
    assert workflow.startswith(
        "\n      - name: Install ci-test Rust components", setup_end
    )
    assert (
        'echo "SOLDR_CACHE_DIR=${{ runner.temp }}/soldr-host-ci/${{ inputs.target }}" '
        '>> "$GITHUB_ENV"'
    ) in domain
    assert (
        'echo "ZCCACHE_CACHE_DIR=${{ runner.temp }}/soldr-host-ci/${{ inputs.target }}'
        '/cache/zccache" >> "$GITHUB_ENV"'
    ) in domain
    assert workflow.index(CANONICAL_CACHE) < workflow.index(CI_TEST_DRIVER)
    assert workflow.index(CANONICAL_CACHE) < workflow.index(GUARD_1838)
    assert workflow.index(CANONICAL_CACHE) < workflow.index(GUARD_1799)
    assert workflow.index(CANONICAL_CACHE) < workflow.index(CI_TEST_RUN)
    assert workflow.index(CANONICAL_CACHE) < workflow.index(
        f"- name: {FINAL_CACHE_STOP}\n"
    )


def test_no_later_host_step_switches_the_canonical_cache_domain() -> None:
    workflow = WORKFLOW.read_text(encoding="utf-8")
    domain = _step_body(workflow, CANONICAL_CACHE)
    later = workflow[workflow.index(domain) + len(domain) :]
    later_lines = [line.lstrip() for line in later.splitlines()]
    assert not any(line.startswith("SOLDR_CACHE_DIR:") for line in later_lines)
    assert not any(line.startswith("ZCCACHE_CACHE_DIR:") for line in later_lines)
    assert not any(CACHE_SHELL_ASSIGNMENT.match(line) for line in later_lines)
    assert not any(
        "GITHUB_ENV" in line
        and any(f"{variable}=" in line for variable in CACHE_ENV_VARS)
        for line in later_lines
    )
    assert 'check_toolchain_homes.py "$SOLDR_CACHE_DIR/logs/builds"' in workflow
