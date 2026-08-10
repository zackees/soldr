from __future__ import annotations

from ci import soldr


def test_cargo_command_falls_back_to_raw_cargo_when_soldr_absent(monkeypatch) -> None:
    """When `soldr` is not on PATH, `cargo_command` returns the raw cargo argv.
    This is the path CI runners take (they use dtolnay/rust-toolchain +
    Swatinem/rust-cache instead of installing soldr)."""
    monkeypatch.setattr("shutil.which", lambda _name: None)
    assert soldr.cargo_command("test", "--workspace") == [
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_routes_through_soldr_when_available(monkeypatch) -> None:
    """When `soldr` is on PATH (typical local dev setup), `cargo_command`
    prefixes the argv with `soldr` so the rustup-managed toolchain is
    resolved instead of whatever stale `cargo` PATH-discovers first."""
    monkeypatch.setattr(
        "shutil.which",
        lambda name: "/usr/local/bin/soldr" if name == "soldr" else None,
    )
    assert soldr.cargo_command("test", "--workspace") == [
        "soldr",
        "cargo",
        "test",
        "--workspace",
    ]


def test_cargo_command_passes_through_any_subcommand(monkeypatch) -> None:
    """Regardless of the subcommand, `cargo_command` only changes the prefix."""
    monkeypatch.setattr("shutil.which", lambda _name: None)
    for subcommand in ("clippy", "fmt", "llvm-cov", "build", "check", "package"):
        assert soldr.cargo_command(subcommand, "--workspace") == [
            "cargo",
            subcommand,
            "--workspace",
        ]


def test_maturin_command_uses_python_module() -> None:
    assert soldr.maturin_command("/tmp/python", "build", "--release") == [
        "/tmp/python",
        "-m",
        "maturin",
        "build",
        "--release",
    ]
