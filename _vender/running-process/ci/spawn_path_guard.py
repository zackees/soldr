from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

PYTHON_PRODUCTION_ROOT = ROOT / "src"
# Scan all Rust source under both crates/ and testbins/. The testbin
# scan exists specifically because issue #115 was a bare
# `Command::spawn` in testbins/src/bin/spawner.rs that survived the
# earlier sanitization round precisely because testbins weren't checked.
RUST_SOURCE_ROOTS = (ROOT / "crates", ROOT / "testbins")

# Wave 7 of #165: paths re-keyed within the merged `running-process`
# crate. Daemon/client/trampoline code that used to live in sibling
# crates now lives at `crates/running-process/src/{daemon,client,bin}/`.
ALLOWED_RUST_COMMAND_NEW = {
    # #648 `rpprobe dump --force`: invokes the external capture tools
    # (py-spy, gdb/lldb, ProcDump, gcore) against an unenrolled target.
    # These are the whole point of the subcommand — the cooperative path
    # needs the target to have enrolled, and this is for the one that did
    # not. Arguments are built by `crate::force` from a numeric pid and a
    # fixed tool name; no user string reaches the command line, and the
    # tool never elevates or changes ptrace_scope.
    Path("crates/running-process-probe-daemon/src/cli/commands.rs"),
    Path("crates/running-process/src/lib.rs"),
    Path("crates/running-process/src/containment.rs"),
    # Inline tests module for running-process lib root.
    Path("crates/running-process/src/tests.rs"),
    # #500 slice 32 (broker-owned bind). These `Command`s are never spawned —
    # they exist only so `InheritableListener::prepare` has something to
    # configure, and the tests then assert on the env var it published and the
    # CLOEXEC bit it cleared. `/bin/true` is a placeholder program name that is
    # never executed. The real spawn, when the launcher wiring lands, goes
    # through `spawn_daemon` like every other backend launch.
    Path("crates/running-process/src/broker/broker_owned_bind/tests.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Python-bindings containment mirror of core's containment.rs.
    Path("crates/running-process-py/src/containment.rs"),
    # Python-bindings inline test fixtures.
    Path("crates/running-process-py/src/tests/pty_process.rs"),
    # Client module (merged from `running-process-client`): auto-starts
    # the daemon process when connecting.
    Path("crates/running-process/src/client/client.rs"),
    # Daemon module (merged from `running-process-daemon`):
    # daemonize / shadow-copy / auto-start.
    # Process-spawn-bearing sub-files only — keep the allowlist narrow.
    Path("crates/running-process/src/daemon/handlers/spawn.rs"),
    Path("crates/running-process/src/daemon/platform/windows.rs"),
    Path("crates/running-process/src/daemon/shadow.rs"),
    # Broker SID-hash bootstrap: derives the per-user identity hash on
    # macOS via `ioreg -d2 -c IOPlatformExpertDevice`. This runs before
    # any broker pipe is bound (the hash is an *input* to the pipe-name
    # derivation), so it cannot route through the broker's own spawn
    # layer. The invocation is a fixed-argument read-only system query
    # with no user input, parsed for the `IOPlatformUUID` line. See
    # `crates/running-process/src/broker/lifecycle/sid.rs` for the
    # full justification in the module docs.
    Path("crates/running-process/src/broker/lifecycle/sid.rs"),
    # systemd KillMode startup probe (Linux only): fixed-argument
    # read-only `systemctl show -p KillMode <unit>` query at daemon
    # startup, before any child is spawned, with no user input. See
    # `crates/running-process/src/systemd_killmode.rs` module docs.
    Path("crates/running-process/src/systemd_killmode.rs"),
    # runpm boot autostart (#427): fixed-argument init-system installers
    # invoked only by `runpm startup`/`unstartup`. The arguments are
    # crate-controlled constants (UNIT_FILENAME, TASK_NAME) joined with
    # a single path to the daemon binary discovered from
    # `std::env::current_exe()` — no user input flows into the argv.
    # See module docs in each file.
    Path("crates/running-process/src/boot_autostart/linux.rs"),
    Path("crates/running-process/src/boot_autostart/macos.rs"),
    Path("crates/running-process/src/boot_autostart/windows.rs"),
    # Broker backend launcher: constructs a reviewed Command only to
    # hand it to the sanitized `spawn_daemon` surface. The module owns
    # service-definition validation, canonical endpoint allocation, and
    # spawned process identity verification before registration.
    Path("crates/running-process/src/broker/server/backend_launcher.rs"),
    # Daemon trampoline binary (merged from `daemon-trampoline`):
    # reads sidecar JSON and spawns the target command.
    Path("crates/running-process/src/bin/trampoline.rs"),
    # Test-only watchdog crate (publish=false, dev-dep only) — invokes
    # procdump.exe via Command::new when the watchdog fires.
    Path("crates/test-watchdog/src/lib.rs"),
    # #637 symbolization worker supervisor: constructs a Command naming only
    # the sibling worker binary (or an operator-supplied override that must
    # already exist), then hands it straight to the sanitized
    # `running_process::spawn::spawn` surface with piped stdio. No user input
    # reaches the program path or the argument list — the capture travels on
    # stdin — and the child is deadline-bounded and killed if it overruns.
    Path("crates/running-process-probe-daemon/src/symbolication.rs"),
    # Testbins (test fixtures): builds Command values to hand to the
    # sanitized spawn surface (or, on Unix, to bare std::Command::spawn
    # because our sanitized spawn calls setpgid in the child, which
    # would break the test's killpg-based containment on macOS).
    Path("testbins/src/bin/spawner.rs"),
    Path("testbins/src/bin/dies_after_spawn.rs"),
    # #551 slice 6e: inject_via_env takes a `&mut Command` from the
    # caller and sets the LD_PRELOAD/DYLD_INSERT_LIBRARIES env var
    # on it before they spawn. The file itself never constructs a
    # Command — the lint trips on a doc-comment example (`Command::
    # new("my-target")` in the `///` rustdoc usage block) and on
    # inline `#[cfg(test)] mod tests` fixtures that construct
    # `/bin/true` for env-setting smoke tests. No production spawn
    # path runs through this module.
    Path("crates/running-process-probe/src/inject_unix.rs"),

}

ALLOWED_RUST_SPAWN = {
    # #636 crash capture sampler: `thread::Builder::spawn` starts an
    # in-process thread that refreshes the preallocated, bounded all-thread
    # snapshot consumed by the native crash callback. It never starts a
    # child process, and the callback itself performs no thread creation.
    Path("crates/running-process-probe/src/crash/mod.rs"),
    # #636 durable crash spool watcher: `thread::Builder::spawn` starts an
    # in-process daemon worker that ingests already-written fixed-size crash
    # records. It never starts a child process.
    Path("crates/running-process-probe-daemon/src/crash_store.rs"),
    # Same crate, same reason: the crash-store tests use `thread::scope`
    # + `scope.spawn` to prove SQLite serializes concurrent writers. These
    # are in-process threads, not child processes. Listed separately
    # because the test module moved into its own file to satisfy the
    # per-file size guard, and this allowlist matches on path.
    Path("crates/running-process-probe-daemon/src/crash_store/tests.rs"),
    # #635 known-stack test: `thread::Builder::spawn` names the fixture
    # `blocked_marker`; this is an in-process thread, not a child process.
    Path("crates/running-process-probe/src/snapshot/mod.rs"),
    # Probe client worker (#633): `thread::Builder::spawn` starts a THREAD,
    # not a process. It exists precisely so `probe::install()` performs no
    # I/O on the calling thread.
    Path("crates/running-process/src/probe/worker.rs"),
    # rpprobed beacon acceptor (#631): `thread::Builder::spawn` starts a
    # THREAD, not a process — it answers beacon identity handshakes while
    # the main loop serves the control socket. Same rationale as the
    # broker-v2 binary entry below.
    Path("crates/running-process-probe-daemon/src/bin/rpprobed.rs"),
    # rpprobed HTTP surface (#642): `thread::Builder::spawn` starts a THREAD
    # hosting the tokio runtime that serves the browser UI and streams
    # artifacts. No child process is created. It runs on its own runtime so an
    # HTTP request storm cannot stall the blocking control-socket accept loop,
    # and vice versa.
    Path("crates/running-process-probe-daemon/src/http/mod.rs"),
    Path("crates/running-process/src/lib.rs"),
    Path("crates/running-process/src/containment.rs"),
    # Inline tests module for running-process lib root.
    Path("crates/running-process/src/tests.rs"),
    # Two-mode spawn surface: `spawn` (contained, sanitized handles, caller stdio)
    # and `spawn_daemon` (detached, NUL stdio, sanitized handles). See #110, #113.
    Path("crates/running-process/src/spawn.rs"),
    # Unix backing impl for `spawn`/`spawn_daemon`: drives bare
    # `Command::spawn()` after applying setpgid/setsid + fd hygiene
    # via `pre_exec`. Windows impl uses CreateProcessW directly so it
    # doesn't trigger this lint.
    Path("crates/running-process/src/spawn_imp_unix.rs"),
    # Native PTY process calls the backend trait's `spawn` method. The
    # backend implementations are reviewed separately below; this is not a
    # raw std::process::Command spawn site.
    Path("crates/running-process/src/pty/native_pty_process.rs"),
    Path("crates/running-process-py/src/lib.rs"),
    # Python-bindings containment mirror of core's containment.rs.
    Path("crates/running-process-py/src/containment.rs"),
    # Python-bindings inline test fixtures.
    Path("crates/running-process-py/src/tests/pty_process.rs"),
    # Client module (merged from `running-process-client`): auto-starts
    # the daemon process when connecting, plus spawns daemon as
    # detached background process.
    Path("crates/running-process/src/client/client.rs"),
    # Daemon handlers (split from former handlers.rs into a module dir).
    # Only sub-files that contain genuine process spawn calls are allowlisted;
    # the remaining sub-files do not need to appear here.
    Path("crates/running-process/src/daemon/handlers/spawn.rs"),
    Path("crates/running-process/src/daemon/handlers/pty_sessions_handlers.rs"),
    Path("crates/running-process/src/daemon/handlers/pipe_sessions_handlers.rs"),
    # Session managers (split from former handlers monolith). Method calls
    # `state.pty_sessions.spawn(...)` / `state.pipe_sessions.spawn(...)` and
    # `PtySessions::spawn` / `PipeSessions::spawn` definitions trigger the
    # `.spawn(` regex; the underlying process spawn happens via the native
    # spawn layer in running-process.
    Path("crates/running-process/src/daemon/pty_sessions.rs"),
    Path("crates/running-process/src/daemon/pipe_sessions.rs"),
    # Daemon server: autostart dispatch invokes the session-manager
    # `.spawn(...)` methods listed above.
    Path("crates/running-process/src/daemon/server.rs"),
    Path("crates/running-process/src/daemon/platform/windows.rs"),
    Path("crates/running-process/src/daemon/shadow.rs"),
    # Daemon trampoline binary (merged from `daemon-trampoline`):
    # reads sidecar JSON and spawns the target command.
    Path("crates/running-process/src/bin/trampoline.rs"),
    # Test-only watchdog crate: spawns procdump.exe with .output()
    # which is .spawn() + wait under the hood.
    Path("crates/test-watchdog/src/lib.rs"),
    # Broker endpoint probe: `thread::Builder::spawn` (a thread, not a
    # process) hosting the blocking local-socket connect so the probe
    # deadline can bound it — interprocess::Stream::connect has no
    # portable timeout and can hang in connect(2) on macOS (#399).
    Path("crates/running-process/src/broker/backend_lifecycle/probe.rs"),
    # Sync IPC client connect deadline (#590 cluster B): `thread::Builder
    # ::spawn` (a thread, not a process) hosting the blocking local-socket
    # connect so `connect_with_timeout` can bound it — same rationale as
    # backend_lifecycle/probe.rs above. No process spawn happens here.
    Path("crates/running-process/src/client/deadline_io.rs"),
    # Broker-v2 binary accept loop: per-connection `thread::Builder::spawn`
    # (a thread, not a process) to handle Hello negotiation under a
    # MAX_INFLIGHT_HANDLERS cap. The thread reads framed bytes from an
    # already-accepted local socket — no process spawn happens. Allowlisted
    # for the same reason as backend_lifecycle/probe.rs above.
    Path("crates/running-process/src/bin/running-process-broker-v2.rs"),
    # #539 slice 2: Windows Job Object IOCP pump uses
    # `thread::Builder::spawn` (a thread, not a process) to drain
    # JOB_OBJECT_MSG_* notifications and forward descendant-lifecycle
    # observer events. No process spawn happens here — same rationale as
    # the broker thread-spawn allowlist entries above.
    Path("crates/running-process/src/windows.rs"),
    # #539 slice 5: Linux descendant pump uses `thread::Builder::spawn`
    # (a thread, not a process) to poll /proc/<pid>/task/<pid>/children
    # and forward descendant-lifecycle observer events. Same rationale
    # as the IOCP pump entry above.
    Path("crates/running-process/src/observer/descendants_linux.rs"),
    # #539 slice 7: macOS descendant pump uses `thread::Builder::spawn`
    # (a thread, not a process) to drain kqueue/EVFILT_PROC events and
    # forward descendant-lifecycle observer events. Same rationale as
    # the IOCP/proc-poll pump entries above.
    Path("crates/running-process/src/observer/descendants_macos.rs"),
    # Testbins: bare std::Command::spawn on Unix only (see comment in
    # testbins/src/bin/spawner.rs — sanitized spawn isn't usable there
    # because of the setpgid-vs-killpg interaction the containment test
    # relies on).
    Path("testbins/src/bin/spawner.rs"),
    # #551 slice 6e: see comment in ALLOWED_RUST_COMMAND_NEW. The
    # `.spawn()` hit is the rustdoc usage example, not production.
    Path("crates/running-process-probe/src/inject_unix.rs"),
}

ALLOWED_PORTABLE_PTY = {
    Path("crates/running-process-py/src/lib.rs"),
    # PTY module moved to core crate
    Path("crates/running-process/src/pty/mod.rs"),
    # PTY backend abstraction: Unix remains the portable-pty backend while
    # Windows routes through the reviewed ConPTY passthrough implementation.
    Path("crates/running-process/src/pty/backend.rs"),
    Path("crates/running-process/src/pty/conpty_passthrough/child.rs"),
    Path("crates/running-process/src/pty/conpty_passthrough/mod.rs"),
    # Native PTY process impl extracted from pty/mod.rs.
    Path("crates/running-process/src/pty/native_pty_process.rs"),
    # Daemon PTY session manager: holds NativePtyProcess handles and reads
    # the child's pid via the underlying portable_pty::Child::process_id.
    # Spawn itself routes through the native layer.
    Path("crates/running-process/src/daemon/pty_sessions.rs"),
}

# `ChildStdin::from` / `ChildStdout::from` / `ChildStderr::from` consumes a
# raw OS handle. Rust's `ChildPipe::read`/`write` on Windows uses
# `alertable_io_internal` (overlapped I/O + alertable `SleepEx`); pairing
# that against a synchronous handle silently drops every write after the
# first — exactly issue #115. The typed `OverlappedHandle::into_child_*`
# wrappers in spawn_imp_windows.rs are the ONLY approved way to do this
# conversion, and they bake in the FILE_FLAG_OVERLAPPED guarantee.
ALLOWED_RUST_CHILD_PIPE_FROM = {
    Path("crates/running-process/src/spawn_imp_windows.rs"),
}

# `CreatePipe` is allowed only for the ConPTY passthrough host/child pipe
# plumbing. Those handles are owned directly by the PTY backend and are not
# wrapped in Rust `ChildStd*`, so they do not trigger the #115 mismatch this
# guard blocks everywhere else.
ALLOWED_RUST_CREATE_PIPE = {
    Path("crates/running-process/src/pty/conpty_passthrough/pipes.rs"),
}

ALLOWED_PYTHON_POPEN = {
    Path("src/running_process/cli.py"),
    # Daemon spawner: subprocess.Popen to launch the trampoline binary
    Path("src/running_process/daemon.py"),
}


def _iter_files(root: Path, suffix: str) -> list[Path]:
    return sorted(path for path in root.rglob(f"*{suffix}") if path.is_file())


def _relative(path: Path) -> Path:
    return path.relative_to(ROOT)


def _find_matches(path: Path, pattern: re.Pattern[str]) -> list[int]:
    lines: list[int] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if pattern.search(line):
            lines.append(line_number)
    return lines


def _format_hits(path: Path, lines: list[int], message: str) -> list[str]:
    rel = _relative(path)
    return [f"{rel}:{line}: {message}" for line in lines]


def check_python_spawn_sites() -> list[str]:
    failures: list[str] = []
    popen_pattern = re.compile(r"\bsubprocess\.Popen\s*\(")
    for path in _iter_files(PYTHON_PRODUCTION_ROOT, ".py"):
        hits = _find_matches(path, popen_pattern)
        if hits and _relative(path) not in ALLOWED_PYTHON_POPEN:
            failures.extend(
                _format_hits(
                    path,
                    hits,
                    "raw subprocess.Popen in production code bypasses native lifecycle enforcement",
                )
            )
    return failures


def check_rust_spawn_sites() -> list[str]:
    failures: list[str] = []
    command_new_pattern = re.compile(r"\bCommand::new\s*\(")
    spawn_pattern = re.compile(r"\.spawn\s*\(")
    portable_pty_pattern = re.compile(r"\bportable_pty\b")
    # Banned outright across the workspace: CreatePipe creates
    # synchronous-only anonymous pipes. Wrapping the parent end in a Rust
    # ChildStd* (whose read uses alertable_io_internal / overlapped I/O)
    # silently drops every write after the first. Use the typed
    # `create_pipe_pair` helper in spawn_imp_windows.rs instead.
    # See issue #115.
    create_pipe_pattern = re.compile(r"\bCreatePipe\s*\(")
    # Bypass detector for the typed-pipe API: only the typed
    # `OverlappedHandle::into_child_*` wrappers in spawn_imp_windows.rs
    # should construct a `ChildStd*` from a raw handle.
    child_pipe_from_pattern = re.compile(r"\bChild(?:Stdin|Stdout|Stderr)::from\s*\(")

    for root in RUST_SOURCE_ROOTS:
        if not root.exists():
            continue
        for path in _iter_files(root, ".rs"):
            rel = _relative(path)
            if "src" not in rel.parts:
                continue

            command_new_hits = _find_matches(path, command_new_pattern)
            if command_new_hits and rel not in ALLOWED_RUST_COMMAND_NEW:
                failures.extend(
                    _format_hits(
                        path,
                        command_new_hits,
                        (
                            "Command::new outside the native spawn layer "
                            "requires review and allowlisting"
                        ),
                    )
                )

            spawn_hits = _find_matches(path, spawn_pattern)
            if spawn_hits and rel not in ALLOWED_RUST_SPAWN:
                failures.extend(
                    _format_hits(
                        path,
                        spawn_hits,
                        "spawn() outside the native spawn layer requires review and allowlisting",
                    )
                )

            portable_pty_hits = _find_matches(path, portable_pty_pattern)
            if portable_pty_hits and rel not in ALLOWED_PORTABLE_PTY:
                failures.extend(
                    _format_hits(
                        path,
                        portable_pty_hits,
                        (
                            "portable_pty usage outside the PTY native layer "
                            "requires review and allowlisting"
                        ),
                    )
                )

            # CreatePipe is banned workspace-wide except the ConPTY passthrough
            # helper allowlisted above. `create_pipe_pair` in
            # spawn_imp_windows.rs uses CreateNamedPipeW + CreateFileW instead
            # and is the only sanctioned construction for Rust ChildStd* pipes.
            create_pipe_hits = _find_matches(path, create_pipe_pattern)
            if create_pipe_hits and rel not in ALLOWED_RUST_CREATE_PIPE:
                failures.extend(
                    _format_hits(
                        path,
                        create_pipe_hits,
                        (
                            "CreatePipe is banned: it returns sync-only handles "
                            "incompatible with Rust's ChildStd* reader; use "
                            "create_pipe_pair (spawn_imp_windows.rs). See #115."
                        ),
                    )
                )

            child_pipe_from_hits = _find_matches(path, child_pipe_from_pattern)
            if child_pipe_from_hits and rel not in ALLOWED_RUST_CHILD_PIPE_FROM:
                failures.extend(
                    _format_hits(
                        path,
                        child_pipe_from_hits,
                        (
                            "ChildStd* construction from a raw handle bypasses the "
                            "typed OverlappedHandle API; route through "
                            "OverlappedHandle::into_child_* in spawn_imp_windows.rs. "
                            "See #115."
                        ),
                    )
                )

    return failures


def main() -> int:
    failures = [
        *check_python_spawn_sites(),
        *check_rust_spawn_sites(),
    ]
    if not failures:
        print("spawn-path guard passed")
        return 0

    print("spawn-path guard failed:")
    for failure in failures:
        print(f"  {failure}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
