from __future__ import annotations

import run_linux_tests


def test_main_delegates_to_linux_docker_debug(monkeypatch) -> None:
    seen: list[list[str]] = []

    monkeypatch.setattr(
        run_linux_tests.linux_docker,
        "main",
        lambda argv: seen.append(list(argv)) or 0,
    )

    result = run_linux_tests.main(["--command", "bash test timeout_does_not_arm_next_expect"])

    assert result == 0
    assert seen == [["debug", "--command", "bash test timeout_does_not_arm_next_expect"]]
