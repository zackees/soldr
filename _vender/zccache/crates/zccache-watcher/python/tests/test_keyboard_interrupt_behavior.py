import _thread
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from zccache.watcher import FileWatcher, handle_keyboard_interrupt


class KeyboardInterruptBehaviorTests(unittest.TestCase):
    def test_handle_keyboard_interrupt_reraises_on_main_thread(self) -> None:
        with self.assertRaises(KeyboardInterrupt) as cm:
            handle_keyboard_interrupt(KeyboardInterrupt("stop"))
        self.assertIsInstance(cm.exception.__cause__, KeyboardInterrupt)

    def test_handle_keyboard_interrupt_notifies_main_thread_from_worker(self) -> None:
        called = []
        worker_error = []

        def worker() -> None:
            try:
                handle_keyboard_interrupt(KeyboardInterrupt("worker"))
            except BaseException as exc:  # pragma: no cover - should stay empty
                worker_error.append(exc)

        with mock.patch.object(
            _thread,
            "interrupt_main",
            side_effect=lambda: called.append(True),
        ):
            thread = threading.Thread(target=worker, name="worker-test")
            thread.start()
            thread.join(timeout=2.0)

        self.assertFalse(worker_error)
        self.assertTrue(called)

    def test_poll_predicate_keyboard_interrupt_propagates(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            file_path = root / "watch.cpp"
            file_path.write_text("a\n")

            watcher = FileWatcher(
                root,
                include_globs=["**/*.cpp"],
                debounce_seconds=0.05,
                poll_interval=0.05,
                notification_predicate=lambda *args, **kwargs: (_ for _ in ()).throw(
                    KeyboardInterrupt()
                ),
            )
            try:
                time.sleep(0.2)
                file_path.write_text("b\n")
                with self.assertRaises(KeyboardInterrupt):
                    deadline = time.time() + 2.0
                    while time.time() < deadline:
                        watcher.poll(0.1)
            finally:
                watcher.stop()

    def test_keyboard_interrupt_stop_logs_once_per_watcher(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            file_path = root / "watch.cpp"
            file_path.write_text("a\n")

            watcher = FileWatcher(
                root,
                include_globs=["**/*.cpp"],
                debounce_seconds=0.05,
                poll_interval=0.05,
                notification_predicate=lambda *args, **kwargs: (_ for _ in ()).throw(
                    KeyboardInterrupt()
                ),
            )
            try:
                time.sleep(0.2)
                with mock.patch("builtins.print") as print_mock:
                    file_path.write_text("b\n")
                    with self.assertRaises(KeyboardInterrupt):
                        deadline = time.time() + 2.0
                        while time.time() < deadline:
                            watcher.poll(0.1)
                    stop_logs = [
                        call
                        for call in print_mock.call_args_list
                        if call.args == ("KeyboardInterrupt: watcher stopped",)
                    ]
                    self.assertEqual(len(stop_logs), 1)
            finally:
                watcher.stop()

    def test_callback_keyboard_interrupt_notifies_main_thread(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            file_path = root / "watch.cpp"
            file_path.write_text("a\n")

            called = []

            def callback(_event) -> None:
                raise KeyboardInterrupt()

            watcher = FileWatcher(
                root,
                include_globs=["**/*.cpp"],
                debounce_seconds=0.05,
                poll_interval=0.05,
                callback=callback,
            )
            try:
                time.sleep(0.2)
                with mock.patch.object(_thread, "interrupt_main", side_effect=lambda: called.append(True)):
                    file_path.write_text("b\n")
                    deadline = time.time() + 2.0
                    while time.time() < deadline and not called:
                        time.sleep(0.05)
                self.assertTrue(called)
            finally:
                watcher.stop()
