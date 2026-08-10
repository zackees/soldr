use pyo3::Python;
use regex::Regex;

use crate::process::NativeRunningProcess;

pub(crate) fn make_test_running_process(py: Python<'_>) -> NativeRunningProcess {
    let cmd = pyo3::types::PyList::new(py, ["echo", "test"]).unwrap();
    NativeRunningProcess::new(
        cmd.as_any(),
        None,
        false,
        true,
        None,
        None,
        true,
        None,
        None,
        "inherit",
        "stdout",
        None,
        false,
    )
    .unwrap()
}

// ── find_expect_match tests ──

#[test]
fn find_expect_match_literal_found() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = make_test_running_process(py);
        let result = process
            .find_expect_match("hello world", "world", None)
            .unwrap();
        assert!(result.is_some());
        let (matched, start, end, groups) = result.unwrap();
        assert_eq!(matched, "world");
        assert_eq!(start, 6);
        assert_eq!(end, 11);
        assert!(groups.is_empty());
    });
}

#[test]
fn find_expect_match_literal_not_found() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = make_test_running_process(py);
        let result = process
            .find_expect_match("hello world", "missing", None)
            .unwrap();
        assert!(result.is_none());
    });
}

#[test]
fn find_expect_match_regex_found() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = make_test_running_process(py);
        let re = Regex::new(r"\d+").unwrap();
        let result = process
            .find_expect_match("hello 123 world", r"\d+", Some(&re))
            .unwrap();
        assert!(result.is_some());
        let (matched, start, end, _) = result.unwrap();
        assert_eq!(matched, "123");
        assert_eq!(start, 6);
        assert_eq!(end, 9);
    });
}

#[test]
fn find_expect_match_regex_with_groups() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = make_test_running_process(py);
        let re = Regex::new(r"(\d+) (\w+)").unwrap();
        let result = process
            .find_expect_match("hello 123 world", r"(\d+) (\w+)", Some(&re))
            .unwrap();
        assert!(result.is_some());
        let (_, _, _, groups) = result.unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], "123");
        assert_eq!(groups[1], "world");
    });
}

#[test]
fn find_expect_match_regex_not_found() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let process = make_test_running_process(py);
        let re = Regex::new(r"\d+").unwrap();
        let result = process
            .find_expect_match("hello world", r"\d+", Some(&re))
            .unwrap();
        assert!(result.is_none());
    });
}

#[test]
#[allow(clippy::invalid_regex)]
fn find_expect_match_invalid_regex_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let result = Regex::new(r"[invalid");
        assert!(result.is_err());
    });
}
