#!/usr/bin/env python3
"""Lifecycle and remote execution for the dockur/macos x86_64 CI guest.

Owner mandate (2026-09-02): no GitHub Actions job may run on a `macos-*`
runner for building or testing. macOS binaries are cross-built on Linux
through `soldr build`; macOS *execution* happens only inside this dockur/macos
x86_64 guest (KVM), hosted on an ordinary `ubuntu-24.04` runner. See
`.github/workflows/_ci-target-run.yml` and CLAUDE.md.

Reference implementation: `../kernal-api/ci/macos-x64/` (guest.sh,
run-in-guest.sh). This module is the soldr equivalent, kept as testable Python
per the repo's "complex CI logic lives in ci/*.py, not inline YAML" rule
rather than as a shell script.

Subcommands:

    preflight [--apply-udev]
        Verify /dev/kvm is readable+writable and docker works. Exit 1 with
        instructions on failure. `--apply-udev` installs the udev rule that
        makes /dev/kvm group-accessible on a hosted GitHub Actions runner
        before checking it (the 2024-04-02 Actions changelog).

    start [--image IMAGE] [--ssh-port PORT] [--ready-timeout SECS] [--name NAME]
        Pull IMAGE (or mount $GUEST_STORAGE for local development against
        dockurr/macos:latest) and boot the guest, then poll ssh until sshd
        actually answers (not just a TCP connect -- sshd resets early in
        boot). Writes $SOLDR_MACOS_GUEST_SSH_KEY to $RUNNER_TEMP/guest_key
        (mode 600) when that secret is set, and uses it as the ssh identity.

    stop [--name NAME] [--logs]
        `docker stop`, ignoring errors. `--logs` also prints the tail of the
        guest's docker logs (useful on failure).

    sync-in --src HOST_DIR --dest GUEST_DIR
        Copy a host directory into the guest with rsync (falling back to scp
        -r if the guest has no rsync), creating the destination first.

    sync-out --src GUEST_DIR --dest HOST_DIR
        The reverse of sync-in.

    exec [--cwd GUEST_DIR] [--env KEY=VALUE ...] -- ARGV...
        Run ARGV inside the guest over ssh with `set -o pipefail`, streaming
        stdout/stderr and propagating the exact remote exit code.

No baked guest image exists yet as of this writing -- see
`ci/macos-x64/README.md` for the one-time manual bootstrap and
`ci/macos-x64/bake.sh` for publishing it to GHCR (soldr#3071 tracks
re-enabling aarch64-apple-darwin execution once an ARM guest story exists).
"""

from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys
import time
from collections.abc import Sequence
from pathlib import Path

DEFAULT_IMAGE = "ghcr.io/zackees/soldr/macos-x64-guest:ventura"
DEFAULT_NAME = "soldr-macos-x86"
DEFAULT_SSH_PORT = 2222
DEFAULT_READY_TIMEOUT = 1800
DEFAULT_USER = "runner"
DEFAULT_HOST = "localhost"

SSH_OPTS: tuple[str, ...] = (
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
)

UDEV_RULE = 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"'

SSH_KEY_ENV = "SOLDR_MACOS_GUEST_SSH_KEY"
GUEST_STORAGE_ENV = "GUEST_STORAGE"

KVM_INSTRUCTIONS = """\
/dev/kvm is not readable+writable by the current user.

On a hosted GitHub Actions Linux runner this is expected -- /dev/kvm exists
but is not world-accessible by default (2024-04-02 Actions changelog). Fix it
with the udev rule this script can apply for you:

    python ci/macos_x64_guest.py preflight --apply-udev

or by hand:

    echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \\
      | sudo tee /etc/udev/rules.d/99-kvm4all.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger --name-match=kvm
"""


def ssh_argv(
    port: int,
    *,
    user: str = DEFAULT_USER,
    host: str = DEFAULT_HOST,
    identity: str | os.PathLike[str] | None = None,
) -> list[str]:
    """The shared ssh prefix argv every guest command is built from."""
    argv = ["ssh", "-p", str(port), *SSH_OPTS]
    if identity is not None:
        argv += ["-i", str(identity)]
    argv.append(f"{user}@{host}")
    return argv


def remote_command_string(
    command: Sequence[str], *, cwd: str | None, env: dict[str, str]
) -> str:
    """Build the exact shell string run on the guest side of ssh.

    `set -o pipefail` so a failing stage of a piped remote command is not
    masked by a trailing success; `cd` and env assignments are shell-quoted
    so paths/values with spaces survive the ssh round trip intact.
    """
    if not command:
        raise ValueError("remote command is required")
    prefix = "set -o pipefail; "
    if cwd:
        prefix += f"cd {shlex.quote(cwd)} && "
    assignments = " ".join(f"{key}={shlex.quote(value)}" for key, value in env.items())
    if assignments:
        prefix += assignments + " "
    return prefix + " ".join(shlex.quote(part) for part in command)


def docker_run_argv(
    *,
    name: str,
    ssh_port: int,
    image: str,
    mount: Sequence[str] = (),
) -> list[str]:
    """The exact `docker run` argv used to boot the guest."""
    return [
        "docker",
        "run",
        "-d",
        "--name",
        name,
        "--device=/dev/kvm",
        "--device=/dev/net/tun",
        "--cap-add",
        "NET_ADMIN",
        "-p",
        f"{ssh_port}:22",
        "-e",
        "VERSION=ventura",
        "-e",
        "RAM_SIZE=8G",
        "-e",
        "CPU_CORES=1",
        "-e",
        "DISK_SIZE=128G",
        *mount,
        "--stop-timeout",
        "120",
        image,
    ]


def _ssh_command_string(
    port: int, *, identity: str | os.PathLike[str] | None = None
) -> str:
    """The `-e "ssh ..."` value rsync's `-e` flag expects, as one shell string.

    Connection options only -- the user@host destination is a separate rsync
    positional argument, added by the caller.
    """
    parts = ["ssh", "-p", str(port), *SSH_OPTS]
    if identity is not None:
        parts += ["-i", str(identity)]
    return " ".join(shlex.quote(part) for part in parts)


def rsync_argv(
    *,
    host_dir: str,
    guest_dir: str,
    port: int,
    direction: str,
    user: str = DEFAULT_USER,
    host: str = DEFAULT_HOST,
    identity: str | os.PathLike[str] | None = None,
) -> list[str]:
    """Build the rsync argv for `direction` "in" (host -> guest) or "out" (guest -> host).

    The source side always gets a trailing slash so rsync copies its
    *contents* into the destination rather than nesting a directory named
    after the source's basename inside it.
    """
    if direction not in {"in", "out"}:
        raise ValueError(f"unsupported sync direction: {direction!r}")
    ssh_cmd = _ssh_command_string(port, identity=identity)
    remote = f"{user}@{host}:{guest_dir}"
    if direction == "in":
        return ["rsync", "-az", "-e", ssh_cmd, f"{host_dir.rstrip('/')}/", remote]
    return ["rsync", "-az", "-e", ssh_cmd, f"{remote.rstrip('/')}/", host_dir]


def scp_fallback_argv(
    *,
    host_dir: str,
    guest_dir: str,
    port: int,
    direction: str,
    user: str = DEFAULT_USER,
    host: str = DEFAULT_HOST,
    identity: str | os.PathLike[str] | None = None,
) -> list[str]:
    """`scp -r` fallback when the guest has no rsync installed."""
    if direction not in {"in", "out"}:
        raise ValueError(f"unsupported sync direction: {direction!r}")
    argv = ["scp", "-r", "-P", str(port), *SSH_OPTS]
    if identity is not None:
        argv += ["-i", str(identity)]
    remote = f"{user}@{host}:{guest_dir}"
    if direction == "in":
        argv += [host_dir, remote]
    else:
        argv += [remote, host_dir]
    return argv


def _identity_path(env: dict[str, str]) -> Path | None:
    ssh_key = env.get(SSH_KEY_ENV)
    if not ssh_key:
        return None
    runner_temp = env.get("RUNNER_TEMP", "/tmp")
    identity = Path(runner_temp) / "guest_key"
    identity.write_text(ssh_key, encoding="utf-8")
    identity.chmod(0o600)
    return identity


def cmd_preflight(args: argparse.Namespace) -> int:
    if args.apply_udev:
        subprocess.run(
            [
                "sudo",
                "sh",
                "-c",
                f"echo '{UDEV_RULE}' > /etc/udev/rules.d/99-kvm4all.rules",
            ],
            check=True,
        )
        subprocess.run(["sudo", "udevadm", "control", "--reload-rules"], check=True)
        subprocess.run(["sudo", "udevadm", "trigger", "--name-match=kvm"], check=True)

    kvm = Path("/dev/kvm")
    if not (os.access(kvm, os.R_OK) and os.access(kvm, os.W_OK)):
        print(KVM_INSTRUCTIONS, file=sys.stderr)
        return 1

    docker = shutil.which("docker")
    if docker is None:
        print("docker is not on PATH", file=sys.stderr)
        return 1
    result = subprocess.run(
        [docker, "info"], capture_output=True, check=False, text=True
    )
    if result.returncode != 0:
        print(f"docker is not usable: {result.stderr.strip()}", file=sys.stderr)
        return 1

    print("kvm ok, docker ok")
    return 0


def cmd_start(args: argparse.Namespace) -> int:
    storage = os.environ.get(GUEST_STORAGE_ENV)
    image = args.image
    mount: list[str] = []
    if storage:
        image = "dockurr/macos:latest"
        mount = ["-v", f"{storage}:/storage"]
    else:
        subprocess.run(["docker", "pull", image], check=True)

    subprocess.run(
        docker_run_argv(
            name=args.name, ssh_port=args.ssh_port, image=image, mount=mount
        ),
        check=True,
    )

    identity = _identity_path(dict(os.environ))
    deadline = time.monotonic() + args.ready_timeout
    while True:
        probe = [*ssh_argv(args.ssh_port, identity=identity), "true"]
        result = subprocess.run(probe, capture_output=True, check=False)
        if result.returncode == 0:
            break
        if time.monotonic() >= deadline:
            subprocess.run(["docker", "logs", "--tail", "60", args.name], check=False)
            print(
                f"guest sshd unreachable on :{args.ssh_port} after "
                f"{args.ready_timeout}s",
                file=sys.stderr,
            )
            return 1
        time.sleep(10)

    print(f"guest ready on :{args.ssh_port}")
    return 0


def cmd_stop(args: argparse.Namespace) -> int:
    if args.logs:
        subprocess.run(["docker", "logs", "--tail", "200", args.name], check=False)
    subprocess.run(["docker", "stop", args.name], check=False, capture_output=True)
    print("guest stopped")
    return 0


def _guest_has_rsync(port: int, identity: Path | None) -> bool:
    probe = [*ssh_argv(port, identity=identity), "command -v rsync"]
    result = subprocess.run(probe, capture_output=True, check=False)
    return result.returncode == 0


def _sync(
    *, host_dir: str, guest_dir: str, port: int, identity: Path | None, direction: str
) -> int:
    if shutil.which("rsync") and _guest_has_rsync(port, identity):
        argv = rsync_argv(
            host_dir=host_dir,
            guest_dir=guest_dir,
            port=port,
            direction=direction,
            identity=identity,
        )
    else:
        argv = scp_fallback_argv(
            host_dir=host_dir,
            guest_dir=guest_dir,
            port=port,
            direction=direction,
            identity=identity,
        )
    subprocess.run(argv, check=True)
    return 0


def cmd_sync_in(args: argparse.Namespace) -> int:
    identity = _identity_path(dict(os.environ))
    mkdir = [
        *ssh_argv(args.ssh_port, identity=identity),
        f"mkdir -p {shlex.quote(args.dest)}",
    ]
    subprocess.run(mkdir, check=True)
    return _sync(
        host_dir=args.src,
        guest_dir=args.dest,
        port=args.ssh_port,
        identity=identity,
        direction="in",
    )


def cmd_sync_out(args: argparse.Namespace) -> int:
    identity = _identity_path(dict(os.environ))
    Path(args.dest).mkdir(parents=True, exist_ok=True)
    return _sync(
        host_dir=args.dest,
        guest_dir=args.src,
        port=args.ssh_port,
        identity=identity,
        direction="out",
    )


def parse_env_pairs(pairs: Sequence[str]) -> dict[str, str]:
    env: dict[str, str] = {}
    for pair in pairs:
        key, sep, value = pair.partition("=")
        if not sep:
            raise ValueError(f"malformed --env value (expected KEY=VALUE): {pair!r}")
        env[key] = value
    return env


def cmd_exec(args: argparse.Namespace) -> int:
    command = args.command
    if command[:1] == ["--"]:
        command = command[1:]
    if not command:
        print("exec requires a command after --", file=sys.stderr)
        return 2
    identity = _identity_path(dict(os.environ))
    env = parse_env_pairs(args.env)
    remote = remote_command_string(command, cwd=args.cwd, env=env)
    argv = [*ssh_argv(args.ssh_port, identity=identity), remote]
    result = subprocess.run(argv, check=False)
    return result.returncode


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    preflight = subparsers.add_parser("preflight", help="verify KVM + docker")
    preflight.add_argument("--apply-udev", action="store_true")
    preflight.set_defaults(func=cmd_preflight)

    start = subparsers.add_parser("start", help="boot the guest")
    start.add_argument("--image", default=DEFAULT_IMAGE)
    start.add_argument("--ssh-port", type=int, default=DEFAULT_SSH_PORT)
    start.add_argument("--ready-timeout", type=int, default=DEFAULT_READY_TIMEOUT)
    start.add_argument("--name", default=DEFAULT_NAME)
    start.set_defaults(func=cmd_start)

    stop = subparsers.add_parser("stop", help="stop the guest")
    stop.add_argument("--name", default=DEFAULT_NAME)
    stop.add_argument("--logs", action="store_true")
    stop.set_defaults(func=cmd_stop)

    sync_in = subparsers.add_parser("sync-in", help="copy a host dir into the guest")
    sync_in.add_argument("--src", required=True)
    sync_in.add_argument("--dest", required=True)
    sync_in.add_argument("--ssh-port", type=int, default=DEFAULT_SSH_PORT)
    sync_in.set_defaults(func=cmd_sync_in)

    sync_out = subparsers.add_parser("sync-out", help="copy a guest dir to the host")
    sync_out.add_argument("--src", required=True, help="guest source directory")
    sync_out.add_argument("--dest", required=True, help="host destination directory")
    sync_out.add_argument("--ssh-port", type=int, default=DEFAULT_SSH_PORT)
    sync_out.set_defaults(func=cmd_sync_out)

    execute = subparsers.add_parser("exec", help="run a command in the guest")
    execute.add_argument("--cwd", default=None)
    execute.add_argument("--env", action="append", default=[])
    execute.add_argument("--ssh-port", type=int, default=DEFAULT_SSH_PORT)
    execute.add_argument("command", nargs=argparse.REMAINDER)
    execute.set_defaults(func=cmd_exec)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
