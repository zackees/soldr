import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def test_root_workspace_loads_process_boundary_dylint() -> None:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    assert "[workspace.metadata.dylint]" in manifest
    assert 'libraries = [{ path = "dylints/*" }]' in manifest
    assert (ROOT / "dylints" / "ban_raw_process_creation" / "src" / "lib.rs").is_file()


def test_required_ci_runs_root_dylint_policy() -> None:
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    assert "Enforce daemon process-creation boundary" in workflow
    # The version is pinned once in `DYLINT_VERSION` and read by both the
    # install lines and the binary cache key, so this asserts the *pin* rather
    # than a literal command string. The literal broke when the installs were
    # cached (they now read `${DYLINT_VERSION}`) even though the pin was intact
    # -- an assertion that fails on a refactor it should not care about is
    # testing the spelling, not the policy.
    assert re.search(
        r"^      DYLINT_VERSION: \d+\.\d+\.\d+$", workflow, re.M
    ), "the dylint version must stay pinned to an exact release in one place"
    assert (
        'soldr cargo install cargo-dylint --version "${DYLINT_VERSION}" --locked'
        in workflow
    )
    assert (
        'soldr cargo install dylint-link --version "${DYLINT_VERSION}" --locked'
        in workflow
    )
    assert "Install Dylint toolchain" in workflow
    assert "soldr rustup toolchain install" in workflow
    assert "--component rustc-dev" in workflow
    assert "--component llvm-tools-preview" in workflow
    assert "--component rust-src" in workflow
    assert "Configure Dylint driver Cargo shim" in workflow
    assert ".github/scripts/configure_dylint_cargo_shim.py" in workflow
    assert "Build daemon process-creation boundary lint" in workflow
    assert "nightly-2026-05-26-x86_64-unknown-linux-gnu" in workflow
    # soldr#2303: the driver cdylibs still build in the release profile (dylint
    # loads them from that path), now carrying the policy exemption marker.
    assert "--profile release  # allow-release:" in workflow
    assert '"${GITHUB_WORKSPACE}/target/dylint/libraries/' in workflow
    assert '"${CARGO_HOME}/bin/cargo-dylint"' in workflow
    assert "dylint --no-build --all" in workflow
    assert "-- --workspace --all-targets" in workflow
    assert "Test daemon process-creation boundary lint" in workflow
    assert "working-directory: dylints/ban_raw_process_creation" in workflow
    # Both process and network boundary lints build and test in the required
    # CI lane, so each owns one nightly build and one UI-test invocation.
    assert workflow.count("soldr rustup run") == 4
    assert (
        "nightly-2026-05-26-x86_64-unknown-linux-gnu\n"
        "          cargo test\n"
        "          --manifest-path Cargo.toml"
    ) in workflow
    assert "--manifest-path Cargo.toml" in workflow
    assert "RUSTUP_TOOLCHAIN: nightly-2026-05-26-x86_64-unknown-linux-gnu" in workflow
    assert workflow.count('SOLDR_NO_GC_TARGET: "1"') == 3
    assert workflow.count("SOLDR_LINKER: default") == 5
    dylint_config = (
        ROOT / "dylints" / "ban_raw_process_creation" / ".cargo" / "config.toml"
    ).read_text(encoding="utf-8")
    assert 'rustflags = ["-C", "linker=dylint-link"]' in dylint_config
    dylint_manifest = (
        ROOT / "dylints" / "ban_raw_process_creation" / "Cargo.toml"
    ).read_text(encoding="utf-8")
    assert "[profile.release]" in dylint_manifest
    assert "opt-level = 0" in dylint_manifest
    assert "lto = false" in dylint_manifest


def test_process_boundary_has_required_ui_fixtures() -> None:
    ui = ROOT / "dylints" / "ban_raw_process_creation" / "ui"
    expected = {
        "disallowed_spawn.rs",
        "disallowed_spawn_ufcs.rs",
        "disallowed_spawn_function_item.rs",
        "disallowed_output.rs",
        "disallowed_status.rs",
        "disallowed_tokio_status.rs",
        "disallowed_creation_flags.rs",
        "disallowed_create_process.rs",
    }
    assert expected <= {path.name for path in ui.glob("*.rs")}
