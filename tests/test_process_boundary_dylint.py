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
    assert re.search(r"^      DYLINT_VERSION: 6\.0\.3$", workflow, re.M)
    assert ".github/scripts/install_catalogued_tools.py" in workflow
    assert "--target x86_64-unknown-linux-gnu" in workflow
    assert '--version "${DYLINT_VERSION}"' in workflow
    assert '--output-dir "${CARGO_HOME}/bin"' in workflow
    assert "cargo-dylint dylint-link" in workflow
    executable = "\n".join(
        line for line in workflow.splitlines() if not line.lstrip().startswith("#")
    )
    assert "cargo install cargo-dylint" not in executable
    assert "cargo install dylint-link" not in executable
    assert "Cache Dylint binaries" not in workflow
    assert "Install Dylint toolchain" in workflow
    assert "soldr rustup toolchain install" in workflow
    assert "--component rustc-dev" in workflow
    assert "--component llvm-tools-preview" in workflow
    assert "--component rust-src" in workflow
    assert "Configure Dylint driver Cargo shim" in workflow
    assert ".github/scripts/configure_dylint_cargo_shim.py" in workflow
    assert "Build daemon process-creation boundary lint" in workflow
    assert "Build local-socket name boundary lint" in workflow
    assert "Enforce running-process local-socket name boundary" in workflow
    assert "Test local-socket name boundary lint" in workflow
    assert "nightly-2026-05-26-x86_64-unknown-linux-gnu" in workflow
    # soldr#2303: the driver cdylibs still build in the release profile (dylint
    # loads them from that path), now carrying the policy exemption marker.
    assert "--profile release  # allow-release:" in workflow
    assert '"${GITHUB_WORKSPACE}/target/dylint/libraries/' in workflow
    assert '"${CARGO_HOME}/bin/cargo-dylint"' in workflow
    assert "dylint --no-build --all" in workflow
    assert "-- --workspace --all-targets" in workflow
    assert "--manifest-path _vender/running-process/Cargo.toml" in workflow
    assert (
        "libban_raw_local_socket_name@"
        "nightly-2026-05-26-x86_64-unknown-linux-gnu.so"
    ) in workflow
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
    assert workflow.count('SOLDR_NO_GC_TARGET: "1"') == 5
    assert workflow.count("SOLDR_LINKER: default") == 8
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
