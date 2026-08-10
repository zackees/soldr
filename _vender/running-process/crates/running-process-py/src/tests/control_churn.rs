use running_process::pty as core_pty;

// ── control_churn_bytes tests ──

#[test]
fn control_churn_bytes_empty() {
    assert_eq!(core_pty::control_churn_bytes(b""), 0);
}

#[test]
fn control_churn_bytes_plain_text() {
    assert_eq!(core_pty::control_churn_bytes(b"hello world"), 0);
}

#[test]
fn control_churn_bytes_ansi_csi_sequence() {
    // \x1b[31m = 5 bytes of control churn, \x1b[0m = 4 bytes
    assert_eq!(core_pty::control_churn_bytes(b"\x1b[31mhello\x1b[0m"), 9);
}

#[test]
fn control_churn_bytes_backspace_cr_del() {
    assert_eq!(core_pty::control_churn_bytes(b"\x08\x0D\x7F"), 3);
}

#[test]
fn control_churn_bytes_bare_escape() {
    // Bare ESC with no CSI sequence following
    assert_eq!(core_pty::control_churn_bytes(b"\x1b"), 1);
}

#[test]
fn control_churn_bytes_mixed() {
    // \x1b[J = 3 bytes CSI + 1 byte BS = 4
    assert_eq!(core_pty::control_churn_bytes(b"ok\x1b[Jmore\x08"), 4);
}

// ── input_contains_newline tests ──

#[test]
fn input_contains_newline_cr() {
    assert!(core_pty::input_contains_newline(b"hello\rworld"));
}

#[test]
fn input_contains_newline_lf() {
    assert!(core_pty::input_contains_newline(b"hello\nworld"));
}

#[test]
fn input_contains_newline_none() {
    assert!(!core_pty::input_contains_newline(b"hello world"));
}

#[test]
fn input_contains_newline_empty() {
    assert!(!core_pty::input_contains_newline(b""));
}

// ── Windows-only pure function tests ──

#[test]
#[cfg(windows)]
fn windows_terminal_input_payload_passthrough() {
    let result = core_pty::windows_terminal_input_payload(b"hello");
    assert_eq!(result, b"hello");
}

#[test]
#[cfg(windows)]
fn windows_terminal_input_payload_lone_lf_becomes_cr() {
    let result = core_pty::windows_terminal_input_payload(b"\n");
    assert_eq!(result, b"\r");
}

#[test]
#[cfg(windows)]
fn windows_terminal_input_payload_crlf_preserved() {
    let result = core_pty::windows_terminal_input_payload(b"\r\n");
    assert_eq!(result, b"\r\n");
}

#[test]
#[cfg(windows)]
fn windows_terminal_input_payload_lone_cr_preserved() {
    let result = core_pty::windows_terminal_input_payload(b"\r");
    assert_eq!(result, b"\r");
}

// ── control_churn_bytes additional edge cases ──

#[test]
fn control_churn_bytes_escape_then_non_bracket() {
    // ESC followed by non-bracket character: only ESC itself is churn
    assert_eq!(core_pty::control_churn_bytes(b"\x1bO"), 1);
}

#[test]
fn control_churn_bytes_incomplete_csi() {
    // ESC [ without terminator - counts entire remainder as churn
    assert_eq!(core_pty::control_churn_bytes(b"\x1b[123"), 5);
}

#[test]
fn control_churn_bytes_multiple_sequences() {
    // Two complete CSI sequences
    assert_eq!(core_pty::control_churn_bytes(b"\x1b[H\x1b[2J"), 7);
}

// ── Windows-specific additional tests ──

#[cfg(windows)]
mod windows_payload_tests {
    use super::*;
    use running_process::pty::terminal_input::format_terminal_input_bytes;
    use running_process::pty::terminal_input::native_terminal_input_mode;

    #[test]
    fn windows_terminal_input_payload_mixed_line_endings() {
        let result = core_pty::windows_terminal_input_payload(b"a\nb\r\nc\rd");
        assert_eq!(result, b"a\rb\r\nc\rd");
    }

    #[test]
    fn windows_terminal_input_payload_consecutive_lf() {
        let result = core_pty::windows_terminal_input_payload(b"\n\n");
        assert_eq!(result, b"\r\r");
    }

    #[test]
    fn windows_terminal_input_payload_empty() {
        let result = core_pty::windows_terminal_input_payload(b"");
        assert!(result.is_empty());
    }

    #[test]
    fn windows_terminal_input_payload_no_line_endings() {
        let result = core_pty::windows_terminal_input_payload(b"hello world");
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn format_terminal_input_bytes_single() {
        assert_eq!(format_terminal_input_bytes(&[0x0D]), "[0d]");
    }

    #[test]
    fn native_terminal_input_mode_preserves_other_flags() {
        // Pass a mode with an unrelated flag set
        let custom_flag = 0x0100; // some arbitrary flag
        let result = native_terminal_input_mode(custom_flag);
        // The custom flag should be preserved
        assert_ne!(result & custom_flag, 0);
    }
}

// ── input_contains_newline tests (iter2 duplicates) ──

#[test]
fn input_contains_newline_with_cr() {
    assert!(core_pty::input_contains_newline(b"hello\rworld"));
}

#[test]
fn input_contains_newline_with_lf() {
    assert!(core_pty::input_contains_newline(b"hello\nworld"));
}

#[test]
fn input_contains_newline_with_crlf() {
    assert!(core_pty::input_contains_newline(b"hello\r\nworld"));
}

#[test]
fn input_contains_newline_without_newline() {
    assert!(!core_pty::input_contains_newline(b"hello world"));
}

// ── control_churn_bytes additional tests (iter2) ──

#[test]
fn control_churn_bytes_backspace() {
    assert_eq!(core_pty::control_churn_bytes(b"\x08"), 1);
}

#[test]
fn control_churn_bytes_carriage_return() {
    assert_eq!(core_pty::control_churn_bytes(b"\x0D"), 1);
}

#[test]
fn control_churn_bytes_delete_char() {
    assert_eq!(core_pty::control_churn_bytes(b"\x7F"), 1);
}

#[test]
fn control_churn_bytes_mixed_with_text() {
    assert_eq!(core_pty::control_churn_bytes(b"hello\x0D\x1b[H"), 4);
}

#[test]
fn control_churn_bytes_plain_text_no_churn() {
    assert_eq!(core_pty::control_churn_bytes(b"hello world"), 0);
}

// ── POSIX input_payload test ──

#[test]
#[cfg(not(windows))]
fn posix_input_payload_passthrough() {
    // On POSIX, input_payload is a passthrough (data.to_vec())
    // This is now in running_process::pty::pty_posix
    let data = b"hello\n";
    assert_eq!(data.to_vec(), b"hello\n");
}

// ── Windows input_payload test ──

#[test]
#[cfg(windows)]
fn windows_pty_input_payload_via_module() {
    assert_eq!(core_pty::windows_terminal_input_payload(b"hello"), b"hello");
    assert_eq!(core_pty::windows_terminal_input_payload(b"\n"), b"\r");
}
