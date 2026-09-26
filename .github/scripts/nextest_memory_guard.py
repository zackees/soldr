"""Memory-aware admission and memory-failure isolation for one Nextest test.

soldr#2885. ``soldr ci-test`` chooses Nextest's test concurrency from measured
CPU and memory headroom, then keeps watching memory while the tests run. This
module is the per-test half of that controller, used by
``nextest_timeout_wrapper.py`` (which Nextest runs around every Unix test):

* **Admission gate.** ci-test's pressure controller creates ``<dir>/paused``
  when available memory falls below its pause mark and removes it only once
  memory recovers past a higher resume mark (hysteresis, with a minimum dwell,
  lives in the controller). A wrapper whose test has not started yet waits
  while the flag exists *and* another test is still running -- with nothing
  running there is nothing to drain, so waiting could only deadlock. Waiters
  are released one at a time, ``RESUME_SPACING_SECS`` apart, so a cleared flag
  cannot readmit every paused test at once and immediately re-trip the mark.
  The wait is bounded (``SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS``) because it
  spends the test's own Nextest timeout budget.

* **Per-test ceiling.** A process-tree memory ceiling
  (``SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES``) turns one pathological test
  into one diagnosed failure instead of a host-wide OOM. On Linux with a
  delegated cgroup v2 root (``SOLDR_NEXTEST_CGROUP_ROOT``) the kernel enforces
  it with ``memory.max`` and ``memory.oom.group``. Otherwise the tree's
  resident set is sampled -- procfs on Linux, ``ps`` on macOS -- and the tree
  is killed once it crosses the ceiling. Sampling can overshoot by whatever the
  tree allocates between samples; that is the documented macOS (and
  undelegated Linux) fallback.

* **Infrastructure classification.** A test that could not start
  (``ENOMEM``/``EAGAIN``), crossed its ceiling, was OOM-killed in its cgroup,
  or failed while printing an allocation-failure signature is reported as an
  infrastructure failure, not an assertion: a named diagnostic, exit status
  ``INFRA_EXIT_CODE``, and an ``<dir>/infra`` record ci-test lists after
  Nextest exits.

The controls are inert unless ci-test sets them, so a plain
``soldr cargo nextest run`` behaves exactly as before. Windows never runs this
wrapper: Nextest places each Windows test in its own Job Object, and no
per-test memory ceiling is applied there.
"""

from __future__ import annotations

import errno
import fcntl
import hashlib
import os
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Mapping

ADMISSION_DIR_ENV = "SOLDR_NEXTEST_ADMISSION_DIR"
MAX_WAIT_ENV = "SOLDR_NEXTEST_ADMISSION_MAX_WAIT_SECS"
CEILING_ENV = "SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES"
SUMMARY_ENV = "SOLDR_NEXTEST_ADMISSION_SUMMARY"
CGROUP_ROOT_ENV = "SOLDR_NEXTEST_CGROUP_ROOT"
# Stripped from the test's own environment: a test that runs a nested Nextest
# must not join (or be counted by) the outer run's admission controller.
CONTROL_ENVS = (
    ADMISSION_DIR_ENV,
    MAX_WAIT_ENV,
    CEILING_ENV,
    SUMMARY_ENV,
    CGROUP_ROOT_ENV,
)

PAUSED_FLAG = "paused"
ACTIVE_DIR = "active"
INFRA_DIR = "infra"
RESUME_LOCK = "resume.lock"

# EX_TEMPFAIL: "a temporary failure ... the user is invited to retry".
INFRA_EXIT_CODE = 75
DEFAULT_MAX_WAIT_SECS = 30.0
RESUME_SPACING_SECS = 0.5
GATE_POLL_SECS = 0.1
LINUX_SAMPLE_SECS = 0.2
PS_SAMPLE_SECS = 1.0
TAIL_BYTES = 64 * 1024
MIB = 1024 * 1024
GIB = 1024 * MIB

_IS_LINUX = sys.platform.startswith("linux")

# Byte signatures an allocation failure leaves in a test's output. Each names
# ENOMEM specifically; a generic "failed" or a panic does not qualify.
_MEMORY_SIGNATURES = (
    b"Cannot allocate memory",
    b"MemoryError",
    b"kind: OutOfMemory",
    b"(os error 12)",
    b"out of memory",
)


def _write_stderr(message: str) -> None:
    sys.stderr.write(message)
    sys.stderr.flush()


def format_bytes(value: int | None) -> str:
    if value is None:
        return "unknown"
    if value >= GIB:
        return f"{value / GIB:.2f} GiB"
    return f"{value / MIB:.1f} MiB"


# ---------------------------------------------------------------------------
# Configuration and identity
# ---------------------------------------------------------------------------


def _positive_number(raw: str | None) -> float | None:
    try:
        value = float((raw or "").strip())
    except ValueError:
        return None
    return value if value > 0 else None


@dataclass(frozen=True)
class GuardConfig:
    """The ci-test controls this wrapper was launched with."""

    admission_dir: Path | None
    max_wait_secs: float
    ceiling_bytes: int | None
    summary: str
    cgroup_root: Path | None

    @classmethod
    def from_env(cls, env: Mapping[str, str]) -> GuardConfig:
        directory = env.get(ADMISSION_DIR_ENV, "").strip()
        ceiling = _positive_number(env.get(CEILING_ENV))
        cgroup = env.get(CGROUP_ROOT_ENV, "").strip()
        return cls(
            admission_dir=Path(directory) if directory else None,
            max_wait_secs=_positive_number(env.get(MAX_WAIT_ENV))
            or DEFAULT_MAX_WAIT_SECS,
            ceiling_bytes=int(ceiling) if ceiling else None,
            summary=env.get(SUMMARY_ENV, "").strip(),
            cgroup_root=Path(cgroup) if cgroup and _IS_LINUX else None,
        )

    @property
    def enabled(self) -> bool:
        return self.admission_dir is not None or self.ceiling_bytes is not None


def identity_of(command: list[str], env: Mapping[str, str]) -> str:
    """Nextest's own ``<binary-id> <test-name>``, or the argv it implies."""

    binary = env.get("NEXTEST_BINARY_ID", "").strip()
    name = env.get("NEXTEST_TEST_NAME", "").strip()
    if binary and name:
        return f"{binary} {name}"
    program = os.path.basename(command[0]) if command else "<unknown>"
    positional = next((arg for arg in command[1:] if not arg.startswith("-")), "")
    return f"{program} {positional}".strip()


def infra_record_name(identity: str) -> str:
    """A file name that round-trips the identity through percent-encoding."""

    encoded = "".join(
        f"%{byte:02X}" if byte in b"%/" or byte < 0x20 or byte == 0x7F else chr(byte)
        for byte in identity.encode("utf-8", errors="replace")
    )
    if len(encoded.encode("utf-8")) <= 200:
        return encoded
    digest = hashlib.sha256(identity.encode("utf-8", errors="replace")).hexdigest()[:16]
    return encoded[:160] + "~" + digest


# ---------------------------------------------------------------------------
# Memory observation (diagnostics only -- policy belongs to soldr ci-test)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class MemoryObservation:
    available_bytes: int | None
    source: str
    cgroup_current_bytes: int | None = None
    cgroup_limit: str | None = None
    pids_current: int | None = None
    pids_limit: str | None = None

    def describe(self) -> str:
        text = f"{format_bytes(self.available_bytes)} ({self.source})"
        if self.cgroup_current_bytes is not None or self.cgroup_limit is not None:
            text += (
                f"; cgroup memory.current={format_bytes(self.cgroup_current_bytes)}"
                f" memory.max={self.cgroup_limit or 'unknown'}"
            )
        return text


def _read(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def _read_int(path: Path) -> int | None:
    raw = _read(path)
    try:
        return int(raw.strip()) if raw is not None else None
    except ValueError:
        return None


def own_cgroup_dir() -> Path | None:
    if not _IS_LINUX:
        return None
    membership = _read(Path("/proc/self/cgroup")) or ""
    for line in membership.splitlines():
        if line.startswith("0::"):
            return Path("/sys/fs/cgroup") / line[3:].strip().lstrip("/")
    return None


def _mem_available(meminfo: str) -> int | None:
    for line in meminfo.splitlines():
        if line.startswith("MemAvailable:"):
            try:
                return int(line.split()[1]) * 1024
            except (IndexError, ValueError):
                return None
    return None


def _vm_stat_available() -> int | None:
    try:
        text = subprocess.run(
            ["/usr/bin/vm_stat"], capture_output=True, text=True, timeout=5, check=False
        ).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    lines = text.splitlines()
    try:
        page_size = int(lines[0].split("page size of ")[1].split()[0])
    except (IndexError, ValueError):
        return None
    pages = 0
    for line in lines[1:]:
        key, _, value = line.partition(":")
        if key.strip() in {
            "Pages free",
            "Pages inactive",
            "Pages speculative",
            "Pages purgeable",
        }:
            try:
                pages += int(value.strip().rstrip("."))
            except ValueError:
                continue
    return pages * page_size


def observe_memory() -> MemoryObservation:
    """The tighter of host ``MemAvailable`` and finite cgroup headroom.

    Mirrors ``soldr ci-test``'s own reading: ``memory.max`` of ``max`` never
    means unlimited (Docker Desktop reports it while the whole VM is short).
    """

    if not _IS_LINUX:
        available = _vm_stat_available() if sys.platform == "darwin" else None
        return MemoryObservation(
            available, "vm_stat" if available is not None else "unavailable"
        )
    available = _mem_available(_read(Path("/proc/meminfo")) or "")
    source = "MemAvailable" if available is not None else "unavailable"
    cgroup = own_cgroup_dir()
    current = limit = pids_current = pids_limit = None
    if cgroup is not None:
        current = _read_int(cgroup / "memory.current")
        limit = (_read(cgroup / "memory.max") or "").strip() or None
        pids_current = _read_int(cgroup / "pids.current")
        pids_limit = (_read(cgroup / "pids.max") or "").strip() or None
        if current is not None and limit and limit.isdigit():
            headroom = max(0, int(limit) - current)
            if available is None or headroom < available:
                available, source = headroom, "cgroup headroom"
    return MemoryObservation(
        available, source, current, limit, pids_current, pids_limit
    )


# ---------------------------------------------------------------------------
# Admission gate
# ---------------------------------------------------------------------------


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OverflowError:
        return False
    return True


def running_tests(admission_dir: Path, exclude_pid: int) -> int:
    count = 0
    try:
        entries = list((admission_dir / ACTIVE_DIR).iterdir())
    except OSError:
        return 0
    for entry in entries:
        try:
            pid = int(entry.name)
        except ValueError:
            continue
        if pid != exclude_pid and _pid_alive(pid):
            count += 1
    return count


def _space_resume(admission_dir: Path) -> None:
    """Release paused waiters one at a time, ``RESUME_SPACING_SECS`` apart."""

    lock_path = admission_dir / RESUME_LOCK
    try:
        fd = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
        first = True
    except FileExistsError:
        try:
            fd = os.open(lock_path, os.O_RDWR)
        except OSError:
            return
        first = False
    except OSError:
        return
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        since = time.time() - os.fstat(fd).st_mtime
        if not first and 0 <= since < RESUME_SPACING_SECS:
            time.sleep(RESUME_SPACING_SECS - since)
        os.utime(fd)
    except OSError:
        pass
    finally:
        os.close(fd)


def await_admission(config: GuardConfig, own_pid: int) -> None:
    """Hold a not-yet-started test while ci-test reports memory pressure."""

    directory = config.admission_dir
    if directory is None:
        return
    paused = directory / PAUSED_FLAG
    started = time.monotonic()
    waited = False

    def blocked() -> bool:
        return paused.exists() and running_tests(directory, own_pid) > 0

    while True:
        if not blocked():
            if not waited:
                return
            # Queue behind earlier waiters, then re-check: pressure may have
            # returned while this test was queued.
            _space_resume(directory)
            if not blocked():
                break
        if time.monotonic() - started >= config.max_wait_secs:
            _write_stderr(
                "nextest memory: admitted after the "
                f"{config.max_wait_secs:g}s pressure wait bound; memory available now: "
                f"{observe_memory().describe()}\n"
            )
            return
        waited = True
        time.sleep(GATE_POLL_SECS)
    _write_stderr(
        "nextest memory: admission paused by memory pressure for "
        f"{time.monotonic() - started:.1f}s\n"
    )


class ActiveSlot:
    """``<dir>/active/<wrapper pid>`` for exactly the life of the test."""

    def __init__(self, admission_dir: Path | None, pid: int) -> None:
        self.path = admission_dir / ACTIVE_DIR / str(pid) if admission_dir else None
        if self.path is not None:
            try:
                self.path.parent.mkdir(parents=True, exist_ok=True)
                self.path.touch()
            except OSError:
                self.path = None

    def release(self) -> None:
        if self.path is not None:
            try:
                self.path.unlink()
            except OSError:
                pass
            self.path = None


# ---------------------------------------------------------------------------
# Process-tree sampling and the per-test ceiling
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class TreeSample:
    rss_bytes: int
    process_count: int
    pids: tuple[int, ...] = ()


def _linux_children(pid: int) -> list[int]:
    children: list[int] = []
    try:
        tasks = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return children
    for task in tasks:
        listing = _read(Path(f"/proc/{pid}/task/{task}/children")) or ""
        children.extend(int(child) for child in listing.split() if child.isdigit())
    return children


def _linux_rss(pid: int) -> int | None:
    statm = _read(Path(f"/proc/{pid}/statm"))
    try:
        return int(statm.split()[1]) * os.sysconf("SC_PAGE_SIZE") if statm else None
    except (IndexError, ValueError):
        return None


def parse_ps_table(text: str) -> dict[int, tuple[int, int]]:
    """``ps -A -o pid=,ppid=,rss=`` -> ``{pid: (ppid, rss_kib)}``."""

    table: dict[int, tuple[int, int]] = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) != 3:
            continue
        try:
            table[int(fields[0])] = (int(fields[1]), int(fields[2]))
        except ValueError:
            continue
    return table


def tree_from_table(root: int, table: dict[int, tuple[int, int]]) -> TreeSample | None:
    if root not in table:
        return None
    children: dict[int, list[int]] = {}
    for pid, (ppid, _) in table.items():
        children.setdefault(ppid, []).append(pid)
    pids, stack = [], [root]
    while stack:
        pid = stack.pop()
        pids.append(pid)
        stack.extend(children.get(pid, []))
    return TreeSample(sum(table[pid][1] for pid in pids) * 1024, len(pids), tuple(pids))


def sample_tree(root: int) -> TreeSample | None:
    """Resident bytes of ``root`` and every live descendant."""

    if not _IS_LINUX:
        try:
            text = subprocess.run(
                ["/bin/ps", "-A", "-o", "pid=,ppid=,rss="],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            ).stdout
        except (OSError, subprocess.TimeoutExpired):
            return None
        return tree_from_table(root, parse_ps_table(text))
    root_rss = _linux_rss(root)
    if root_rss is None:
        return None
    total, pids, stack, seen = 0, [], [root], set()
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        rss = root_rss if pid == root else _linux_rss(pid)
        if rss is None:
            continue
        total += rss
        pids.append(pid)
        stack.extend(_linux_children(pid))
    return TreeSample(total, len(pids), tuple(pids))


def kill_tree(root: int) -> None:
    """SIGKILL ``root``'s process group and every descendant still visible."""

    sample = sample_tree(root)
    try:
        os.killpg(root, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass
    for pid in sample.pids if sample else ():
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass


class TreeMonitor(threading.Thread):
    """Sample the test's tree; kill it once it crosses the sampled ceiling."""

    def __init__(self, root: int, ceiling_bytes: int | None) -> None:
        super().__init__(name="nextest-memory-monitor", daemon=True)
        self.root = root
        self.ceiling_bytes = ceiling_bytes
        self.interval = LINUX_SAMPLE_SECS if _IS_LINUX else PS_SAMPLE_SECS
        self.peak = TreeSample(0, 0)
        self.exceeded: TreeSample | None = None
        self._stop_requested = threading.Event()

    def _observe(self) -> bool:
        sample = sample_tree(self.root)
        if sample is None:
            return False
        if sample.rss_bytes > self.peak.rss_bytes:
            self.peak = sample
        if self.ceiling_bytes is not None and sample.rss_bytes > self.ceiling_bytes:
            self.exceeded = sample
            kill_tree(self.root)
            return False
        return True

    def run(self) -> None:
        while self._observe() and not self._stop_requested.wait(self.interval):
            pass

    def stop(self) -> None:
        self._stop_requested.set()
        self.join(timeout=5)


@dataclass(frozen=True)
class CgroupOutcome:
    peak_bytes: int | None
    oom_kills: int


@dataclass
class CgroupCeiling:
    """A kernel-enforced per-test ceiling below a delegated cgroup v2 root."""

    path: Path
    ceiling_bytes: int
    joined: bool = field(default=False)

    @classmethod
    def create(
        cls, root: Path, ceiling_bytes: int, owner_pid: int
    ) -> CgroupCeiling | None:
        controllers = (_read(root / "cgroup.subtree_control") or "").split()
        if "memory" not in controllers:
            return None
        for attempt in range(8):
            leaf = root / (
                f"snt-{owner_pid}" if attempt == 0 else f"snt-{owner_pid}-{attempt}"
            )
            try:
                leaf.mkdir()
            except FileExistsError:
                continue
            except OSError:
                return None
            try:
                (leaf / "memory.max").write_text(str(ceiling_bytes), encoding="ascii")
            except OSError:
                cls(leaf, ceiling_bytes).release()
                return None
            try:
                (leaf / "memory.oom.group").write_text("1", encoding="ascii")
            except OSError:
                pass
            return cls(leaf, ceiling_bytes)
        return None

    def join_current_process(self) -> None:
        """Run in the forked child before exec: move it into the leaf."""

        try:
            fd = os.open(self.path / "cgroup.procs", os.O_WRONLY)
            try:
                os.write(fd, b"0")
            finally:
                os.close(fd)
        except OSError:
            pass

    def confirm(self, pid: int) -> bool:
        procs = (_read(self.path / "cgroup.procs") or "").split()
        self.joined = str(pid) in procs
        return self.joined

    def outcome(self) -> CgroupOutcome:
        kills = 0
        for line in (_read(self.path / "memory.events") or "").splitlines():
            name, _, value = line.partition(" ")
            if name in {"oom_kill", "oom_group_kill"} and value.strip().isdigit():
                kills += int(value)
        return CgroupOutcome(_read_int(self.path / "memory.peak"), kills)

    def release(self) -> None:
        try:
            self.path.rmdir()
            return
        except OSError:
            pass
        # Detached descendants (daemons a test left behind) still live here.
        # Park them in one unlimited sibling so the per-test leaf can go.
        orphans = self.path.parent / "snt-orphans"
        try:
            orphans.mkdir(exist_ok=True)
            for pid in (_read(self.path / "cgroup.procs") or "").split():
                (orphans / "cgroup.procs").write_text(pid, encoding="ascii")
            self.path.rmdir()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Output tail and classification
# ---------------------------------------------------------------------------


class OutputTail:
    """The last ``limit`` bytes a stream produced, for signature matching."""

    def __init__(self, limit: int = TAIL_BYTES) -> None:
        self.limit = limit
        self._data = bytearray()
        self._lock = threading.Lock()

    def feed(self, chunk: bytes) -> None:
        with self._lock:
            self._data += chunk
            if len(self._data) > self.limit:
                del self._data[: len(self._data) - self.limit]

    def bytes(self) -> bytes:
        with self._lock:
            return bytes(self._data)


def memory_signature(text: bytes) -> str | None:
    for signature in _MEMORY_SIGNATURES:
        if signature in text:
            return signature.decode("ascii")
    if b"memory allocation of " in text and b" failed" in text:
        return "memory allocation of <n> bytes failed"
    return None


def _describe_status(returncode: int | None) -> str:
    if returncode is None:
        return "not started"
    if returncode < 0:
        try:
            return f"killed by {signal.Signals(-returncode).name}"
        except ValueError:
            return f"killed by signal {-returncode}"
    return str(returncode)


class ProcessGuard:
    """Everything the wrapper needs around one test process."""

    def __init__(self, command: list[str], env: Mapping[str, str]) -> None:
        self.config = GuardConfig.from_env(env)
        self.identity = identity_of(command, env)
        self.own_pid = os.getpid()
        self.at_admission: MemoryObservation | None = None
        self.slot: ActiveSlot | None = None
        self.monitor: TreeMonitor | None = None
        self.cgroup: CgroupCeiling | None = None
        self.stdout_tail = OutputTail()
        self.stderr_tail = OutputTail()

    # -- lifecycle ---------------------------------------------------------

    def child_env(self, env: dict[str, str]) -> dict[str, str]:
        for name in CONTROL_ENVS:
            env.pop(name, None)
        return env

    def before_spawn(self) -> None:
        if not self.config.enabled:
            return
        await_admission(self.config, self.own_pid)
        self.at_admission = observe_memory()
        self.slot = ActiveSlot(self.config.admission_dir, self.own_pid)
        if (
            self.config.cgroup_root is not None
            and self.config.ceiling_bytes is not None
        ):
            self.cgroup = CgroupCeiling.create(
                self.config.cgroup_root, self.config.ceiling_bytes, self.own_pid
            )

    def wrap_preexec(
        self, preexec: Callable[[], None] | None
    ) -> Callable[[], None] | None:
        cgroup = self.cgroup
        if cgroup is None:
            return preexec

        def joined() -> None:
            cgroup.join_current_process()
            if preexec is not None:
                preexec()

        return joined

    def after_spawn(self, pid: int) -> None:
        if not self.config.enabled:
            return
        sampled_ceiling = self.config.ceiling_bytes
        if self.cgroup is not None:
            if self.cgroup.confirm(pid):
                sampled_ceiling = None  # the kernel enforces memory.max
            else:
                _write_stderr(
                    "nextest memory: could not join the per-test cgroup; "
                    "falling back to the sampled ceiling\n"
                )
        self.monitor = TreeMonitor(pid, sampled_ceiling)
        self.monitor.start()

    def finish(self, returncode: int, terminated: bool = False) -> int:
        """Classify the exited test; return the status the wrapper reports.

        A test Nextest itself terminated (its timeout) keeps Nextest's verdict.
        """

        if self.monitor is not None:
            self.monitor.stop()
        outcome = self.cgroup.outcome() if self.cgroup is not None else None
        try:
            cause = None if terminated else self._classify(returncode, outcome)
            if cause is None:
                return returncode
            self._report(cause, returncode, outcome)
            return INFRA_EXIT_CODE
        finally:
            if self.cgroup is not None:
                self.cgroup.release()
            if self.slot is not None:
                self.slot.release()

    def spawn_failed(self, error: OSError) -> int:
        if self.slot is not None:
            self.slot.release()
        if self.cgroup is not None:
            self.cgroup.release()
        hint = {
            errno.ENOMEM: "ENOMEM: memory exhaustion",
            errno.EAGAIN: "EAGAIN: PID or memory pressure",
        }.get(error.errno or 0, "operating-system resource failure")
        self._report(f"could not start the test process: {error} ({hint})", None, None)
        return INFRA_EXIT_CODE

    # -- classification and reporting -------------------------------------

    def _classify(self, returncode: int, outcome: CgroupOutcome | None) -> str | None:
        if self.monitor is not None and self.monitor.exceeded is not None:
            return (
                "process tree exceeded the per-test memory ceiling "
                f"{format_bytes(self.config.ceiling_bytes)} (sampled RSS "
                f"{format_bytes(self.monitor.exceeded.rss_bytes)}); terminated its process tree"
            )
        if outcome is not None and outcome.oom_kills > 0 and self.cgroup is not None:
            return (
                "the kernel OOM-killed the test inside its per-test cgroup "
                f"(memory.max={format_bytes(self.config.ceiling_bytes)}, {self.cgroup.path})"
            )
        if returncode != 0 and self.config.admission_dir is not None:
            signature = memory_signature(self.stderr_tail.bytes()) or memory_signature(
                self.stdout_tail.bytes()
            )
            if signature is not None:
                return f"memory-exhaustion signature {signature!r} in the test's output"
        return None

    def _report(
        self, cause: str, returncode: int | None, outcome: CgroupOutcome | None
    ) -> None:
        at_failure = observe_memory()
        peak = self.monitor.peak if self.monitor is not None else None
        if outcome is not None and outcome.peak_bytes is not None:
            peak_text = f"{format_bytes(outcome.peak_bytes)} (cgroup memory.peak)"
        elif peak is not None and peak.process_count:
            peak_text = f"{format_bytes(peak.rss_bytes)} across {peak.process_count} process(es)"
        else:
            peak_text = "not observable"
        if self.cgroup is not None and self.cgroup.joined:
            ceiling = (
                f"{format_bytes(self.config.ceiling_bytes)} (cgroup v2 memory.max)"
            )
        elif self.config.ceiling_bytes is not None:
            ceiling = (
                f"{format_bytes(self.config.ceiling_bytes)} (sampled process-tree RSS)"
            )
        else:
            ceiling = "none"
        tree = peak.process_count if peak is not None else 0
        pids = (
            f"cgroup pids.current={at_failure.pids_current} pids.max={at_failure.pids_limit}"
            if at_failure.pids_current is not None
            else "cgroup pids unavailable"
        )
        lines = [
            "",
            "=== nextest memory: infrastructure failure, not a test assertion ===",
            f"test: {self.identity}",
            f"cause: {cause}",
            f"admission: {self.config.summary or 'no soldr ci-test admission summary'}",
            f"per-test memory ceiling: {ceiling}",
            f"process-tree peak RSS: {peak_text}",
            "memory available at admission: "
            + (self.at_admission.describe() if self.at_admission else "not measured"),
            f"memory available at failure: {at_failure.describe()}",
            f"pid pressure: {tree} process(es) in the test tree; {pids}",
            f"original exit status: {_describe_status(returncode)}",
            "=== nextest memory: only this test's process tree was affected ===",
            "",
        ]
        _write_stderr("\n".join(lines))
        if self.config.admission_dir is not None:
            record = (
                self.config.admission_dir / INFRA_DIR / infra_record_name(self.identity)
            )
            try:
                record.parent.mkdir(parents=True, exist_ok=True)
                record.touch()
            except OSError:
                pass
