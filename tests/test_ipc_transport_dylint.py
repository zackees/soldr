from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
LINT = ROOT / "dylints" / "ban_raw_ipc_transport"


def test_raw_ipc_transport_dylint_is_wired_and_documents_blessed_adapters() -> None:
    source = (LINT / "src" / "lib.rs").read_text(encoding="utf-8")
    readme = (LINT / "README.md").read_text(encoding="utf-8")
    workflow = (ROOT / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")

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

    assert "Build raw IPC transport boundary lint" in workflow
    assert "Test raw IPC transport boundary lint" in workflow


def test_session_listener_uses_one_facade_and_explicit_platform_delegates() -> None:
    source = (
        ROOT / "crates" / "soldr-daemon" / "src" / "daemon" / "session_endpoint.rs"
    ).read_text(encoding="utf-8")
    assert source.count("fn bind_session_listener(") == 1
    assert source.count("fn bind_session_listener_impl(") == 4
    for platform in ("windows", "linux", "macos"):
        assert f'#[cfg(target_os = "{platform}")]' in source
    assert "#[cfg(any(" not in source
    assert 'not(target_os = "windows")' in source
    assert 'not(target_os = "linux")' in source
    assert 'not(target_os = "macos")' in source
