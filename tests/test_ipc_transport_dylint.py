from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LINT = ROOT / "dylints" / "ban_raw_ipc_transport"


def test_raw_ipc_transport_dylint_is_wired_and_documents_blessed_adapters() -> None:
    source = (LINT / "src" / "lib.rs").read_text(encoding="utf-8")
    readme = (LINT / "README.md").read_text(encoding="utf-8")
    plan = (ROOT / "crates" / "soldr-cli" / "src" / "ci_test" / "plan.rs").read_text(
        encoding="utf-8"
    )
    workflow = (ROOT / ".github" / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )

    for constructor in (
        "ListenerOptions",
        "ConnectOptions",
        "UnixListener",
        "UnixStream",
        "UnixDatagram",
        "ServerOptions",
        "ClientOptions",
    ):
        assert constructor in source

    for adapter in (
        "daemon/client.rs",
        "daemon/server.rs",
        "daemon/ipc_peer.rs",
        "daemon/session_endpoint.rs",
        "broker_server.rs",
        "broker_spawn.rs",
        "broker_control_transport_",
        "session_transport.rs",
    ):
        assert adapter in readme

    assert '"ban_raw_ipc_transport"' in plan
    assert 'format!("dylint-test-{lint}")' in plan
    assert workflow.count("name: Run prescribed host validation") == 1
    assert "ci-test --target" in workflow


def test_session_listener_uses_one_facade_and_explicit_platform_delegates() -> None:
    # soldr#2493 moved the per-platform delegate bodies into the
    # soldr-platform concrete trees: the daemon keeps a single facade
    # call and each concrete tree owns one host implementation with no
    # cfg duplication.
    source = (
        ROOT / "crates" / "soldr-daemon" / "src" / "daemon" / "session_endpoint.rs"
    ).read_text(encoding="utf-8")
    assert source.count("fn bind_session_listener(") == 1
    assert "bind_owner_only_listener(socket_path)" in source
    assert source.count("#[cfg(") == source.count("#[cfg(test)]")
    for tree_name in ("platform_win", "platform_linux", "platform_macos"):
        tree = (
            ROOT
            / "crates"
            / "soldr-platform"
            / "src"
            / tree_name
            / "ipc"
            / "listener.rs"
        ).read_text(encoding="utf-8")
        assert tree.count("fn bind_owner_only_listener(") == 1, tree_name
