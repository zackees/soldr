//! Python bindings for the host window icon (#577).
//!
//! Thin: the capability probe and the OS calls live in `running_process`, so
//! this only translates arguments and errors.
//!
//! # The capability has to cross the boundary too
//!
//! The whole point of the Rust API is that most terminals silently ignore an
//! icon change, so support is reported rather than assumed. A binding that
//! exposed only "set it" and swallowed the verdict would hand Python callers
//! the very trap the Rust side exists to avoid — they would call it, get no
//! exception, and ship a feature that does nothing on the default terminal of
//! every recent Windows install. So `native_window_icon_support` is exposed
//! alongside, and the setter raises rather than returning quietly.

use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use running_process::window_icon::{self, IconError, IconScope, IconSource, StockIcon};

/// Why the host cannot accept an icon, or `None` when it can.
///
/// Returning the reason rather than a bare bool is deliberate: a caller that
/// only learns "no" has nothing to log, and nothing to distinguish "this
/// terminal never allows it" from "this process has no console right now".
#[pyfunction]
#[pyo3(signature = (pid=None))]
pub(crate) fn native_window_icon_support(pid: Option<u32>) -> Option<&'static str> {
    window_icon::icon_support(scope_of(pid)).reason()
}

/// `None` means this process's own window; a pid names a child's.
///
/// Modelled as an optional pid rather than two functions because the caller's
/// question is the same either way — only the window differs — and a caller
/// that already has a pid should not have to find a different entry point.
fn scope_of(pid: Option<u32>) -> IconScope {
    match pid {
        None => IconScope::Host,
        Some(pid) => IconScope::Child { pid },
    }
}

/// Set the host console window's icon from a `.ico` file.
///
/// Raises `RuntimeError` when the host cannot accept an icon and `OSError`
/// when the file cannot be loaded — different problems with different
/// remedies, so they are different exception types.
#[pyfunction]
#[pyo3(signature = (path, pid=None))]
pub(crate) fn native_set_window_icon_from_path(path: &str, pid: Option<u32>) -> PyResult<()> {
    window_icon::set_icon(scope_of(pid), &IconSource::Path(path.into())).map_err(to_py_error)
}

/// Set the host console window's icon from `.ico` bytes.
///
/// Takes the data by value rather than a path so an application can embed its
/// icon in the wheel and never depend on a file existing at runtime — the case
/// a packaged Python app actually has.
///
/// Raises the same exceptions as the path form, plus `ValueError` when the
/// bytes are not a usable icon: that is the caller's data being wrong, which
/// is a different problem from the file system or the terminal.
#[pyfunction]
#[pyo3(signature = (data, pid=None))]
pub(crate) fn native_set_window_icon_from_bytes(data: Vec<u8>, pid: Option<u32>) -> PyResult<()> {
    window_icon::set_icon(scope_of(pid), &IconSource::Bytes(data)).map_err(to_py_error)
}

/// Names accepted by [`native_set_window_icon_stock`], in declaration order.
///
/// Exposed so the Python layer can build its enum from one list rather than
/// repeating the names and letting the two drift.
pub(crate) const STOCK_ICON_NAMES: [&str; 5] =
    ["application", "warning", "error", "information", "shield"];

/// Message for a name this build does not know.
///
/// Built as a plain `String` rather than inside the `PyErr` so it can be
/// asserted without an initialized interpreter — formatting a `PyErr` needs
/// the GIL, which a plain unit-test run does not have.
fn unknown_stock_message(name: &str) -> String {
    format!(
        "unknown stock icon {name:?}; expected one of {}",
        STOCK_ICON_NAMES.join(", ")
    )
}

fn parse_stock(name: &str) -> Option<StockIcon> {
    match name {
        "application" => Some(StockIcon::Application),
        "warning" => Some(StockIcon::Warning),
        "error" => Some(StockIcon::Error),
        "information" => Some(StockIcon::Information),
        "shield" => Some(StockIcon::Shield),
        _ => None,
    }
}

/// Set the host console window's icon to one the OS already provides.
///
/// Takes a name rather than an integer because Python has no way to reference
/// the Rust enum, and an unknown name raises `ValueError` **listing the valid
/// ones** — a bare "invalid icon" would leave a caller guessing at a set they
/// cannot enumerate.
#[pyfunction]
#[pyo3(signature = (name, pid=None))]
pub(crate) fn native_set_window_icon_stock(name: &str, pid: Option<u32>) -> PyResult<()> {
    let stock =
        parse_stock(name).ok_or_else(|| PyValueError::new_err(unknown_stock_message(name)))?;
    window_icon::set_icon(scope_of(pid), &IconSource::Stock(stock)).map_err(to_py_error)
}

/// The stock icon names this build accepts.
#[pyfunction]
pub(crate) fn native_stock_icon_names() -> Vec<&'static str> {
    STOCK_ICON_NAMES.to_vec()
}

/// Which Python exception an [`IconError`] becomes.
///
/// Split from the conversion so the mapping is testable without an
/// initialized interpreter — `PyErr::is_instance_of` needs one, and a plain
/// `cargo test` has none, which would leave this decision unverified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    /// The terminal will never accept an icon: not retryable, and not caused
    /// by the caller's input.
    Unsupported,
    /// The supplied icon data is malformed. The caller's input, and fixable
    /// by supplying different bytes — so a `ValueError`, not an `OSError`,
    /// which would suggest the file system or the OS was at fault.
    BadData,
    /// Something else about loading the icon failed.
    Os,
}

fn classify(error: &IconError) -> ErrorKind {
    match error {
        IconError::Unsupported { .. } => ErrorKind::Unsupported,
        IconError::Decode(_) => ErrorKind::BadData,
        _ => ErrorKind::Os,
    }
}

fn to_py_error(error: IconError) -> PyErr {
    match classify(&error) {
        ErrorKind::Unsupported => PyRuntimeError::new_err(error.to_string()),
        ErrorKind::BadData => PyValueError::new_err(error.to_string()),
        ErrorKind::Os => PyOSError::new_err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer wherever it runs, including a CI box with no
    /// console window at all.
    #[test]
    fn support_is_reportable_everywhere() {
        match native_window_icon_support(None) {
            None => {} // available
            Some(reason) => assert!(!reason.is_empty(), "a refusal must explain itself"),
        }
    }

    /// The binding must agree with the Rust API it wraps, not carry its own
    /// idea of what is supported.
    #[test]
    fn the_binding_agrees_with_the_rust_verdict() {
        let rust = window_icon::host_icon_support();
        assert_eq!(native_window_icon_support(None), rust.reason());
        assert_eq!(
            native_window_icon_support(None).is_none(),
            rust.is_available()
        );
    }

    /// Every advertised name must parse. A name in the list that the parser
    /// rejects would raise `ValueError` for a caller who did exactly what the
    /// error message told them to.
    #[test]
    fn every_advertised_stock_name_parses() {
        for name in STOCK_ICON_NAMES {
            assert!(
                parse_stock(name).is_some(),
                "{name} is advertised but unparsable"
            );
        }
        assert_eq!(native_stock_icon_names(), STOCK_ICON_NAMES.to_vec());
    }

    /// Distinct names must not collapse onto one variant.
    #[test]
    fn stock_names_map_to_distinct_variants() {
        let mut seen = Vec::new();
        for name in STOCK_ICON_NAMES {
            let variant = parse_stock(name).unwrap();
            assert!(
                !seen.contains(&variant),
                "{name} duplicates an earlier variant"
            );
            seen.push(variant);
        }
    }

    /// An unknown name must name the alternatives, not just refuse.
    ///
    /// A caller cannot enumerate the valid set from Python, so "invalid icon"
    /// alone would leave them guessing.
    #[test]
    fn an_unknown_stock_name_lists_the_valid_ones() {
        let text = unknown_stock_message("sparkle");
        assert!(text.contains("sparkle"), "should echo the bad name: {text}");
        for name in STOCK_ICON_NAMES {
            assert!(text.contains(name), "should list {name}: {text}");
        }
    }

    /// A pid that owns no console window must be refused, and never confused
    /// with the host — the whole reason the scope is threaded through.
    #[test]
    fn a_childless_pid_is_unsupported_even_when_the_host_is_not() {
        // pid 0 never owns a console window, on any machine.
        assert!(native_window_icon_support(Some(0)).is_some());
        assert!(
            native_set_window_icon_from_path("anything.ico", Some(0)).is_err(),
            "a pid with no console cannot take an icon"
        );
    }

    /// Passing no pid must mean the host, not "some process".
    #[test]
    fn an_absent_pid_selects_the_host() {
        assert_eq!(scope_of(None), IconScope::Host);
        assert_eq!(scope_of(Some(7)), IconScope::Child { pid: 7 });
        assert_eq!(
            native_window_icon_support(None),
            window_icon::host_icon_support().reason()
        );
    }

    /// Malformed bytes are the caller's data, not an OS fault.
    #[test]
    fn bad_icon_data_maps_to_a_value_error() {
        use running_process::window_icon::ico::IcoError;
        assert_eq!(
            classify(&IconError::Decode(IcoError::NotAnIcon)),
            ErrorKind::BadData
        );
    }

    /// Garbage must be refused whatever the host: an unsupported terminal
    /// rejects it first, a supported one fails to decode. Never `Ok`.
    #[test]
    fn garbage_bytes_never_report_success() {
        assert!(
            native_set_window_icon_from_bytes(vec![0xFF; 64], None).is_err(),
            "garbage is not an icon"
        );
    }

    /// The two failure kinds must map to different exceptions, because the
    /// remedies differ: an unsupported terminal is permanent, a bad icon is
    /// the caller's to fix.
    ///
    /// Asserted on the classification rather than the built `PyErr`, so it
    /// runs without an initialized interpreter.
    #[test]
    fn an_unsupported_host_and_a_bad_icon_map_to_different_exceptions() {
        assert_eq!(
            classify(&IconError::Unsupported { reason: "no" }),
            ErrorKind::Unsupported
        );
        assert_eq!(
            classify(&IconError::Load {
                path: "x.ico".into(),
                source: std::io::Error::other("boom"),
            }),
            ErrorKind::Os
        );
    }

    /// Whatever the host, a nonexistent file never reports success.
    #[test]
    fn a_missing_file_never_reports_success() {
        assert!(
            native_set_window_icon_from_path("no-such-icon-file.ico", None).is_err(),
            "a missing file cannot produce a set icon"
        );
    }
}
