use pyo3::IntoPyObject;
use running_process::pty as core_pty;
use running_process::{CommandSpec, StderrMode, StdinMode, StreamKind};

use crate::helpers::{parse_command, stderr_mode, stdin_mode, stream_kind};

// ── parse_command tests ──

#[test]
fn parse_command_string_with_shell() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = pyo3::types::PyString::new(py, "echo hello");
        let result = parse_command(cmd.as_any(), true).unwrap();
        assert!(matches!(result, CommandSpec::Shell(ref s) if s == "echo hello"));
    });
}

#[test]
fn parse_command_string_without_shell_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = pyo3::types::PyString::new(py, "echo hello");
        let result = parse_command(cmd.as_any(), false);
        assert!(result.is_err());
    });
}

#[test]
fn parse_command_list_without_shell() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = pyo3::types::PyList::new(py, ["echo", "hello"]).unwrap();
        let result = parse_command(cmd.as_any(), false).unwrap();
        assert!(matches!(result, CommandSpec::Argv(ref v) if v.len() == 2));
    });
}

#[test]
fn parse_command_list_with_shell_joins() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = pyo3::types::PyList::new(py, ["echo", "hello"]).unwrap();
        let result = parse_command(cmd.as_any(), true).unwrap();
        assert!(matches!(result, CommandSpec::Shell(ref s) if s == "echo hello"));
    });
}

#[test]
fn parse_command_empty_list_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = pyo3::types::PyList::empty(py);
        let result = parse_command(cmd.as_any(), false);
        assert!(result.is_err());
    });
}

#[test]
fn parse_command_invalid_type_errors() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let cmd = 42i32.into_pyobject(py).unwrap();
        let result = parse_command(cmd.as_any(), false);
        assert!(result.is_err());
    });
}

// ── stream_kind tests ──

#[test]
fn stream_kind_stdout() {
    let result = stream_kind("stdout").unwrap();
    assert_eq!(result, StreamKind::Stdout);
}

#[test]
fn stream_kind_stderr() {
    let result = stream_kind("stderr").unwrap();
    assert_eq!(result, StreamKind::Stderr);
}

#[test]
fn stream_kind_invalid() {
    let result = stream_kind("invalid");
    assert!(result.is_err());
}

// ── stdin_mode tests ──

#[test]
fn stdin_mode_inherit() {
    assert_eq!(stdin_mode("inherit").unwrap(), StdinMode::Inherit);
}

#[test]
fn stdin_mode_piped() {
    assert_eq!(stdin_mode("piped").unwrap(), StdinMode::Piped);
}

#[test]
fn stdin_mode_null() {
    assert_eq!(stdin_mode("null").unwrap(), StdinMode::Null);
}

#[test]
fn stdin_mode_invalid() {
    assert!(stdin_mode("invalid").is_err());
}

// ── stderr_mode tests ──

#[test]
fn stderr_mode_stdout() {
    assert_eq!(stderr_mode("stdout").unwrap(), StderrMode::Stdout);
}

#[test]
fn stderr_mode_pipe() {
    assert_eq!(stderr_mode("pipe").unwrap(), StderrMode::Pipe);
}

#[test]
fn stderr_mode_invalid() {
    assert!(stderr_mode("invalid").is_err());
}

// ── command_builder_from_argv tests ──

#[test]
fn command_builder_from_argv_single_arg() {
    let argv = vec!["echo".to_string()];
    let _cmd = core_pty::command_builder_from_argv(&argv);
    // Just ensure it doesn't panic
}

#[test]
fn command_builder_from_argv_multi_args() {
    let argv = vec!["echo".to_string(), "hello".to_string(), "world".to_string()];
    let _cmd = core_pty::command_builder_from_argv(&argv);
    // Just ensure it doesn't panic
}
