"""Tests for the host window icon API (#577)."""

import unittest

from running_process import window_icon


class TestIconSupport(unittest.TestCase):
    """The capability report, which is the point of the API."""

    def test_support_answers_everywhere(self):
        # Must not raise on a CI box with no console window, on macOS, or
        # anywhere else.
        reason = window_icon.icon_support()
        if reason is not None:
            self.assertTrue(reason, "a refusal must explain itself")
            self.assertIsInstance(reason, str)

    def test_is_supported_agrees_with_the_reason(self):
        reason = window_icon.icon_support()
        self.assertEqual(window_icon.is_supported(), reason is None)


class TestSetHostIcon(unittest.TestCase):
    """Failure behavior, which is what callers must be able to rely on."""

    def test_never_reports_success_for_a_missing_file(self):
        # Whatever the host: an unsupported terminal raises
        # IconUnsupportedError, a supported one fails to load the file. What
        # must never happen is returning None as though it worked.
        with self.assertRaises((window_icon.IconUnsupportedError, OSError)):
            window_icon.set_host_icon("no-such-icon-file.ico")

    def test_unsupported_host_raises_the_typed_error(self):
        if window_icon.is_supported():
            self.skipTest("this host accepts icons; the refusal path needs one that does not")
        with self.assertRaises(window_icon.IconUnsupportedError):
            window_icon.set_host_icon("anything.ico")

    def test_unsupported_error_is_a_runtime_error(self):
        # Callers that only catch RuntimeError should still catch this.
        self.assertTrue(issubclass(window_icon.IconUnsupportedError, RuntimeError))


class TestDegradedBuild(unittest.TestCase):
    """A build without the native symbols must degrade legibly."""

    def test_support_reports_a_reason_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            reason = window_icon.icon_support()
            self.assertIsNotNone(reason)
            self.assertIn("no window-icon support", reason)
            self.assertFalse(window_icon.is_supported())
        finally:
            window_icon._native_module = original

    def test_setting_raises_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            with self.assertRaises(window_icon.IconUnsupportedError):
                window_icon.set_host_icon("anything.ico")
        finally:
            window_icon._native_module = original


if __name__ == "__main__":
    unittest.main()


class TestSetHostIconFromBytes(unittest.TestCase):
    """The embedded-icon path (#577)."""

    def test_garbage_bytes_never_report_success(self):
        # An unsupported terminal refuses first; a supported one fails to
        # decode. Returning None as though it worked is the one wrong answer.
        with self.assertRaises(
            (window_icon.IconUnsupportedError, ValueError, OSError)
        ):
            window_icon.set_host_icon_from_bytes(b"\xff" * 64)

    def test_empty_bytes_never_report_success(self):
        with self.assertRaises(
            (window_icon.IconUnsupportedError, ValueError, OSError)
        ):
            window_icon.set_host_icon_from_bytes(b"")

    def test_raises_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            with self.assertRaises(window_icon.IconUnsupportedError):
                window_icon.set_host_icon_from_bytes(b"whatever")
        finally:
            window_icon._native_module = original

    def test_accepts_a_bytearray(self):
        # Callers reading from a file or a resource loader get bytearray or
        # memoryview; the conversion happens here rather than at every call.
        with self.assertRaises(
            (window_icon.IconUnsupportedError, ValueError, OSError)
        ):
            window_icon.set_host_icon_from_bytes(bytearray(b"\xff" * 32))


class TestStockIcons(unittest.TestCase):
    """Stock icons (#577)."""

    def test_python_enum_matches_the_native_list(self):
        # The two lists must not drift: a name Python offers that the native
        # layer rejects would raise ValueError for a caller who used the enum.
        from running_process import _native

        native_names = set(_native.native_stock_icon_names())
        python_names = {member.value for member in window_icon.StockIcon}
        self.assertEqual(python_names, native_names)

    def test_stock_icon_is_a_str_subclass(self):
        # So a caller passing the bare string keeps working.
        self.assertIsInstance(window_icon.StockIcon.WARNING, str)
        self.assertEqual(window_icon.StockIcon.WARNING, "warning")

    def test_unknown_name_raises_value_error_listing_options(self):
        with self.assertRaises(ValueError) as caught:
            window_icon.set_host_icon_stock("sparkle")
        message = str(caught.exception)
        self.assertIn("sparkle", message)
        self.assertIn("warning", message)

    def test_a_valid_stock_icon_never_reports_a_bogus_error(self):
        # On an unsupported host this raises IconUnsupportedError; on a
        # supported one it succeeds. What it must never do is claim the name
        # was invalid.
        try:
            window_icon.set_host_icon_stock(window_icon.StockIcon.WARNING)
        except window_icon.IconUnsupportedError:
            pass
        except ValueError as exc:
            self.fail(f"a valid stock icon was reported invalid: {exc}")

    def test_raises_when_native_is_absent(self):
        original = window_icon._native_module
        window_icon._native_module = lambda: None
        try:
            with self.assertRaises(window_icon.IconUnsupportedError):
                window_icon.set_host_icon_stock(window_icon.StockIcon.ERROR)
        finally:
            window_icon._native_module = original


class TestChildScope(unittest.TestCase):
    """Targeting a child's console window by pid (#577)."""

    def test_a_childless_pid_is_unsupported(self):
        # pid 0 never owns a console window on any platform.
        self.assertIsNotNone(window_icon.icon_support(0))
        self.assertFalse(window_icon.is_supported(0))

    def test_omitting_pid_means_the_host(self):
        self.assertEqual(window_icon.icon_support(), window_icon.icon_support(None))

    def test_setters_accept_a_pid_and_refuse_a_childless_one(self):
        for call in (
            lambda: window_icon.set_host_icon("x.ico", 0),
            lambda: window_icon.set_host_icon_from_bytes(b"\xff" * 32, 0),
            lambda: window_icon.set_host_icon_stock(window_icon.StockIcon.WARNING, 0),
        ):
            with self.assertRaises(
                (window_icon.IconUnsupportedError, ValueError, OSError)
            ):
                call()

    def test_a_childless_pid_is_reported_even_where_the_host_is_supported(self):
        # The scope must actually reach the native layer: if pid were ignored,
        # this would mirror the host's answer instead of its own.
        if not window_icon.is_supported():
            self.skipTest("host is unsupported here; this needs a supported host")
        self.assertFalse(
            window_icon.is_supported(0),
            "pid 0 has no console window even when the host does",
        )
