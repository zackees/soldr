"""Tests for the Python probe client (#634)."""

import os
import threading
import time
import unittest

import pytest

from running_process import probe

requires_probe = pytest.mark.skipif(
    not probe.is_available(),
    reason="this build of _native has no probe support",
)

requires_snapshot = pytest.mark.skipif(
    not probe.snapshot_supported(),
    reason="native stack capture is not implemented on this platform",
)

# A socket that deliberately does not exist. Enrollment must succeed anyway —
# an absent daemon is a normal condition the worker retries through — so this
# keeps the tests independent of whether a daemon happens to be running.
NO_DAEMON = "\\\\.\\pipe\\rp-probe-test-nonexistent"


class TestProbeConfig(unittest.TestCase):
    """Config defaults, which are security-relevant."""

    def test_env_values_are_deny_by_default(self):
        config = probe.ProbeConfig(app_class="t")
        self.assertEqual(config.env_allowlist, [])
        self.assertFalse(config.disclose_cwd)

    def test_allowlists_are_not_shared_between_configs(self):
        # A mutable default would let one config's opt-in leak into every
        # other config in the process.
        a = probe.ProbeConfig(app_class="a")
        b = probe.ProbeConfig(app_class="b")
        a.env_allowlist.append("SECRET")
        self.assertEqual(b.env_allowlist, [])


@requires_probe
class TestProbeInstall(unittest.TestCase):
    """Enrollment lifecycle against an absent daemon."""

    def test_install_does_not_block(self):
        start = time.monotonic()
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        elapsed = time.monotonic() - start
        self.addCleanup(guard.close)

        # The target is well under a millisecond; the bound is loose so a
        # loaded CI runner does not make this flaky. It still fails outright
        # if enrollment ever waits on the daemon.
        self.assertLess(
            elapsed,
            1.0,
            f"install() took {elapsed:.3f}s; enrollment must not do I/O",
        )

    def test_install_succeeds_without_a_daemon(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        self.addCleanup(guard.close)
        self.assertIsNotNone(guard)
        self.assertIsNotNone(guard.handle)
        # Enrolling is not the same as being registered: no daemon answered.
        self.assertFalse(guard.is_armed())

    def test_close_is_idempotent(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        self.assertTrue(guard.close(), "the first close releases the guard")
        self.assertFalse(guard.close(), "a second close releases nothing")
        self.assertIsNone(guard.handle)

    def test_closed_guard_is_not_armed(self):
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        guard.close()
        self.assertFalse(guard.is_armed())

    def test_context_manager_closes(self):
        with probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        ) as guard:
            self.assertIsNotNone(guard.handle)
        self.assertIsNone(guard.handle)

    def test_guards_are_independent(self):
        first = probe.install(
            probe.ProbeConfig(app_class="a", socket_override=NO_DAEMON)
        )
        second = probe.install(
            probe.ProbeConfig(app_class="b", socket_override=NO_DAEMON)
        )
        self.addCleanup(second.close)

        self.assertNotEqual(first.handle, second.handle)
        first.close()
        self.assertIsNotNone(second.handle, "closing one guard must not close another")
        self.assertTrue(second.close())

    def test_close_from_another_thread_releases_once(self):
        # atexit and an explicit close can race; exactly one must win.
        guard = probe.install(
            probe.ProbeConfig(app_class="t", socket_override=NO_DAEMON)
        )
        results = []

        def closer():
            results.append(guard.close())

        threads = [threading.Thread(target=closer) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        self.assertEqual(
            sum(1 for r in results if r),
            1,
            f"exactly one close should do the release, got {results}",
        )


@requires_probe
@requires_snapshot
class TestMixedModeSnapshot(unittest.TestCase):
    """Native and interpreter frames for the same OS thread (#634)."""

    def test_snapshot_reports_this_processes_threads(self):
        captured = probe.snapshot()
        self.assertGreater(len(captured.threads), 0, "a live process has threads")
        for os_tid, dump in captured.threads.items():
            self.assertEqual(os_tid, dump.os_tid)

    def test_the_calling_thread_has_python_frames(self):
        # A thread cannot suspend itself, so the caller contributes no native
        # frames — but its Python frames must still be present, which is the
        # case that would break if the two views were merged by position.
        captured = probe.snapshot()
        me = threading.get_native_id()
        self.assertIn(
            me, captured.threads, f"calling thread {me} missing from {list(captured.threads)}"
        )
        self.assertTrue(
            captured.threads[me].python,
            "the calling thread must contribute interpreter frames",
        )

    def test_python_frames_name_this_test(self):
        # Proves the interpreter frames belong to the thread they are filed
        # under, rather than being an arbitrary non-empty list.
        me = probe.snapshot().threads[threading.get_native_id()]
        functions = [f.name for f in me.python]
        self.assertIn(
            "test_python_frames_name_this_test",
            functions,
            f"own frame absent from {functions}",
        )

    def test_a_thread_blocked_in_rust_reports_both_stacks(self):
        """The acceptance case: mixed-mode capture of a native-blocked thread.

        The worker parks inside a real native call, so its machine stack is
        down in `_native` while its Python stack still shows the call that got
        it there. Both must appear, filed under the same OS thread id.

        The blocking call must *release* the GIL, which is what any well-behaved
        one does — a native call that blocks while holding the GIL freezes the
        whole interpreter, so no snapshot could run at all. (`_native`'s
        `native_test_hang_in_rust` is deliberately the GIL-holding kind: it
        exists to be dumped by an external debugger, and using it here
        deadlocks.)
        """
        from running_process import _native

        detector = _native.NativeIdleDetector(
            30.0,  # timeout_seconds — long enough to still be blocked
            30.0,  # stability_window_seconds
            0.05,  # sample_interval_seconds
            _native.NativeSignalBool(True),
        )
        worker_tid: list[int] = []
        entered = threading.Event()

        def worker():
            worker_tid.append(threading.get_native_id())
            entered.set()
            detector.wait(20.0)

        thread = threading.Thread(target=worker, daemon=True)
        thread.start()
        try:
            self.assertTrue(entered.wait(timeout=30), "worker never started")
            # Give the thread time to get down into the native wait rather
            # than still be on its way there.
            time.sleep(0.5)
            self.assertTrue(worker_tid, "worker never recorded its tid")

            captured = probe.snapshot()
            tid = worker_tid[0]
            self.assertIn(
                tid,
                captured.threads,
                f"blocked thread {tid} missing from {list(captured.threads)}",
            )

            dump = captured.threads[tid]
            self.assertTrue(
                dump.native,
                "a thread parked in a native call must yield native frames",
            )
            self.assertTrue(
                dump.python,
                "the same thread must still yield its interpreter frames",
            )
            self.assertTrue(dump.is_mixed())
            self.assertIn(
                "worker",
                [f.name for f in dump.python],
                "the Python half must show the call that entered native code",
            )
        finally:
            detector.mark_exit(0, False)
            thread.join(timeout=30)


class TestProbeUnavailable(unittest.TestCase):
    """Degrading when the wheel was built without probe support."""

    def test_install_returns_none_when_unavailable(self):
        original = probe._native_module
        probe._native_module = lambda: None
        try:
            result = probe.install(probe.ProbeConfig(app_class="t"))
            self.assertIsNone(result, "a probe-less build degrades rather than raising")
        finally:
            probe._native_module = original

    def test_required_turns_unavailability_into_an_error(self):
        original = probe._native_module
        probe._native_module = lambda: None
        try:
            with self.assertRaises(probe.ProbeUnavailableError):
                probe.install(probe.ProbeConfig(app_class="t"), required=True)
        finally:
            probe._native_module = original


if __name__ == "__main__":
    unittest.main()


@requires_probe
@requires_snapshot
class TestWriteDump(unittest.TestCase):
    """The mixed-mode artifact (#713)."""

    def _dump(self, tmp):
        from pathlib import Path

        return probe.write_dump(reason="test", dump_dir=Path(tmp))

    def test_writes_metadata_and_a_stacks_artifact(self):
        import json
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmp:
            metadata_path = self._dump(tmp)
            self.assertTrue(metadata_path.is_file())

            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            self.assertEqual(metadata["reason"], "test")
            self.assertEqual(metadata["pid"], os.getpid())
            self.assertGreater(metadata["thread_count"], 0)

            stacks_path = Path(tmp) / metadata["artifacts"][0]
            self.assertTrue(stacks_path.is_file(), "the named artifact must exist")

    def test_the_artifact_carries_both_halves_keyed_by_tid(self):
        import json
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmp:
            metadata_path = self._dump(tmp)
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            payload = json.loads(
                (Path(tmp) / metadata["artifacts"][0]).read_text(encoding="utf-8")
            )

            self.assertEqual(payload["pid"], os.getpid())
            self.assertEqual(payload["runtime"], "python")

            by_tid = {t["os_tid"]: t for t in payload["threads"]}
            me = by_tid[threading.get_native_id()]
            self.assertTrue(me["python"], "the caller must contribute Python frames")
            self.assertIn(
                "test_the_artifact_carries_both_halves_keyed_by_tid",
                [f["func"] for f in me["python"]],
                "the Python half must describe this very test",
            )
            # At least one thread should carry native frames; the caller
            # cannot, since a thread cannot suspend itself.
            self.assertTrue(
                any(t["native"] for t in payload["threads"]),
                "no thread reported native frames",
            )

    def test_native_frames_are_module_relative_and_hex(self):
        import json
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmp:
            metadata_path = self._dump(tmp)
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            payload = json.loads(
                (Path(tmp) / metadata["artifacts"][0]).read_text(encoding="utf-8")
            )

            frames = [f for t in payload["threads"] for f in t["native"]]
            self.assertTrue(frames, "expected some native frames")
            for frame in frames:
                self.assertTrue(
                    frame["offset"].startswith("0x"),
                    f"offsets should be hex for symbolizers; got {frame['offset']}",
                )
                int(frame["offset"], 16)

            # The point of the change: an attributed frame names a module in
            # the artifact's own module list, so the offset can be resolved
            # against that binary later.
            attributed = [f for f in frames if f["attributed"]]
            self.assertTrue(
                attributed,
                "no frame was attributed to a module; the artifact cannot be symbolized",
            )
            for frame in attributed:
                self.assertIsNotNone(frame["module_index"])
                self.assertLess(frame["module_index"], len(payload["modules"]))

    def test_the_artifact_lists_the_modules_its_frames_reference(self):
        import json
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmp:
            metadata_path = self._dump(tmp)
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            payload = json.loads(
                (Path(tmp) / metadata["artifacts"][0]).read_text(encoding="utf-8")
            )

            self.assertTrue(payload["modules"], "expected referenced modules")
            for module in payload["modules"]:
                self.assertTrue(module["name"])
                # The path is what makes the capture symbolizable: the symbol
                # file lives beside the binary.
                self.assertIsNotNone(module["path"], f"{module['name']} has no path")
            self.assertEqual(metadata["module_count"], len(payload["modules"]))

    def test_snapshot_attributes_frames_to_modules(self):
        captured = probe.snapshot()
        frames = [f for d in captured.threads.values() for f in d.native]
        self.assertTrue(frames, "expected native frames")

        attributed = [f for f in frames if f.is_attributed()]
        self.assertTrue(attributed, "no frame matched any loaded module")
        for frame in attributed:
            module = captured.module_of(frame)
            self.assertIsNotNone(module)
            self.assertTrue(module.name)

    def test_the_artifact_states_its_scope(self):
        # The artifact must not be mistakable for another process's stacks,
        # nor its missing symbol names read as a failure.
        import json
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as tmp:
            metadata_path = self._dump(tmp)
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            payload = json.loads(
                (Path(tmp) / metadata["artifacts"][0]).read_text(encoding="utf-8")
            )
            self.assertIn("calling process", payload["scope"])
            self.assertIn("unsymbolized", payload["native_frames"])
