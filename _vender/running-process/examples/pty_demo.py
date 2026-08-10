#!/usr/bin/env python3
"""PTY Demo - Shows the difference between pipe and PTY output."""

import sys

from running_process import RunningProcess


def demo_pipe_vs_pty():
    """Demonstrate the difference between pipe and PTY output."""
    print("PTY Support Demo")
    print("=" * 50)

    # Check if PTY is available
    test_proc = RunningProcess(["echo", "test"], use_pty=True, auto_run=False)
    pty_available = test_proc.use_pty

    print(f"Platform: {sys.platform}")
    print(f"PTY Available: {pty_available}")
    print()

    if not pty_available:
        print("PTY is not available on this platform.")
        if sys.platform == "win32":
            print("To enable PTY support on Windows, install winpty:")
            print("  pip install winpty")
        return

    # Command that behaves differently with PTY
    if sys.platform == "win32":
        command = ["cmd", "/c", "echo Hello from PTY!"]
    else:
        # Unix command that checks for TTY
        command = [
            "sh",
            "-c",
            (
                "if [ -t 0 ]; then echo 'Running in TTY mode'; "
                "else echo 'Running in pipe mode'; fi"
            ),
        ]

    print("Running with pipe (standard mode):")
    proc_pipe = RunningProcess(command, use_pty=False)
    exit_code = proc_pipe.wait()
    print(f"Output: {proc_pipe.stdout.strip()}")
    print(f"Exit code: {exit_code}")
    print()

    print("Running with PTY:")
    proc_pty = RunningProcess(command, use_pty=True)
    exit_code = proc_pty.wait()
    print(f"Output: {proc_pty.stdout.strip()}")
    print(f"Exit code: {exit_code}")
    print()

    # Demo ANSI filtering
    print("Demo: ANSI escape sequence filtering")
    if sys.platform == "win32":
        # Windows doesn't typically output ANSI by default, so simulate it
        ansi_command = ["echo", "\x1b[31mRed Text\x1b[0m"]
    else:
        # Unix with ANSI colors
        ansi_command = ["echo", "-e", "\x1b[31mRed Text\x1b[0m"]

    print("Running command with ANSI codes using PTY:")
    proc_ansi = RunningProcess(ansi_command, use_pty=True)
    exit_code = proc_ansi.wait()
    print("Raw output would contain ANSI codes, but PTY filtered them:")
    print(f"Clean output: '{proc_ansi.stdout.strip()}'")
    print()

    print("PTY demo completed successfully!")


if __name__ == "__main__":
    demo_pipe_vs_pty()
