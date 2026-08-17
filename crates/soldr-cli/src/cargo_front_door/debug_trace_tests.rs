use super::*;

#[test]
fn disabled_values_are_recognized() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for value in ["0", "false", "no", "off", "", "  "] {
        let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, value);
        assert!(!enabled(), "{value:?} must not enable tracing");
    }
    let _guard = crate::EnvVarGuard::remove(DEBUG_TRACE_ENV_VAR);
    assert!(!enabled(), "unset must not enable tracing");
}

#[test]
fn truthy_values_enable_tracing() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for value in ["1", "true", "on", "yes"] {
        let _guard = crate::EnvVarGuard::set(DEBUG_TRACE_ENV_VAR, value);
        assert!(enabled(), "{value:?} must enable tracing");
    }
}

#[test]
fn json_strings_escape_quotes_backslashes_and_control_characters() {
    assert_eq!(json_string("plain"), r#""plain""#);
    assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
    assert_eq!(json_string(r"C:\cargo"), r#""C:\\cargo""#);
    assert_eq!(json_string("a\nb\tc"), r#""a\nb\tc""#);
    assert_eq!(json_string("\u{1}"), r#""\u0001""#);
}

#[test]
fn rendered_argv_joins_program_and_args() {
    let mut command = std::process::Command::new("cargo");
    command.args(["build", "--target", "x86_64-pc-windows-gnu"]);
    assert_eq!(
        render_argv(&command),
        "cargo build --target x86_64-pc-windows-gnu"
    );
}

/// soldr#2546 acceptance: with the flag absent, spawning through the traced
/// path performs no tracing work — it is exactly `Command::spawn`.
#[test]
fn spawn_traced_without_the_flag_only_spawns() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _guard = crate::EnvVarGuard::remove(DEBUG_TRACE_ENV_VAR);
    let mut command =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };
    let mut child = spawn_traced(&mut command, "test noop").expect("spawn");
    let status = child.wait().expect("wait");
    assert!(status.success());
}
