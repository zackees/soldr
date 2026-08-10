use running_process::pty::terminal_input::TerminalInputEventRecord;
#[cfg(all(windows, test))]
use running_process::pty::terminal_input::NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV;
#[cfg(windows)]
use running_process::pty::terminal_input::{
    control_character_for_unicode, format_terminal_input_bytes, native_terminal_input_mode,
    native_terminal_input_trace_target, repeat_terminal_input_bytes, repeated_modified_sequence,
    repeated_tilde_sequence, terminal_input_modifier_parameter, translate_console_key_event,
};

#[cfg(windows)]
use winapi::um::wincon::{
    ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
};
#[cfg(windows)]
use winapi::um::wincontypes::{
    KEY_EVENT_RECORD, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, SHIFT_PRESSED,
};
#[cfg(windows)]
use winapi::um::winuser::{VK_RETURN, VK_TAB, VK_UP};

#[cfg(all(windows, test))]
use crate::helpers::with_locked_env_var;
use crate::terminal_input::{NativeTerminalInput, NativeTerminalInputEvent};

#[cfg(windows)]
pub(crate) fn key_event(
    virtual_key_code: u16,
    unicode: u16,
    control_key_state: u32,
    repeat_count: u16,
) -> KEY_EVENT_RECORD {
    let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
    event.bKeyDown = 1;
    event.wRepeatCount = repeat_count;
    event.wVirtualKeyCode = virtual_key_code;
    event.wVirtualScanCode = 0;
    event.dwControlKeyState = control_key_state;
    unsafe {
        *event.uChar.UnicodeChar_mut() = unicode;
    }
    event
}

#[test]
#[cfg(windows)]
fn native_terminal_input_mode_disables_cooked_console_flags() {
    let original_mode =
        ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE;

    let active_mode = native_terminal_input_mode(original_mode);

    assert_eq!(active_mode & ENABLE_ECHO_INPUT, 0);
    assert_eq!(active_mode & ENABLE_LINE_INPUT, 0);
    assert_eq!(active_mode & ENABLE_PROCESSED_INPUT, 0);
    assert_eq!(active_mode & ENABLE_QUICK_EDIT_MODE, 0);
    assert_ne!(active_mode & ENABLE_EXTENDED_FLAGS, 0);
    assert_ne!(active_mode & ENABLE_WINDOW_INPUT, 0);
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_preserves_submit_hint_for_enter() {
    let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
        .expect("enter should translate");
    assert_eq!(event.data, b"\r");
    assert!(event.submit);
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_keeps_shift_enter_non_submit() {
    let event =
        translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, SHIFT_PRESSED, 1))
            .expect("shift-enter should translate");
    // Shift+Enter emits CSI u sequence so downstream apps can
    // distinguish it from plain Enter.
    assert_eq!(event.data, b"\x1b[13;2u");
    assert!(!event.submit);
    assert!(event.shift);
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_encodes_shift_tab() {
    let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, SHIFT_PRESSED, 1))
        .expect("shift-tab should translate");
    assert_eq!(event.data, b"\x1b[Z");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_encodes_modified_arrows() {
    let event = translate_console_key_event(&key_event(
        VK_UP as u16,
        0,
        SHIFT_PRESSED | LEFT_CTRL_PRESSED,
        1,
    ))
    .expect("modified arrow should translate");
    assert_eq!(event.data, b"\x1b[1;6A");
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_encodes_alt_printable_with_escape_prefix() {
    let event =
        translate_console_key_event(&key_event(b'X' as u16, 'x' as u16, LEFT_ALT_PRESSED, 1))
            .expect("alt printable should translate");
    assert_eq!(event.data, b"\x1bx");
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_encodes_ctrl_printable_as_control_character() {
    let event =
        translate_console_key_event(&key_event(b'C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("ctrl-c should translate");
    assert_eq!(event.data, [0x03]);
}

#[test]
#[cfg(windows)]
fn translate_terminal_input_ignores_keyup_events() {
    let mut event = key_event(VK_RETURN as u16, '\r' as u16, 0, 1);
    event.bKeyDown = 0;
    assert!(translate_console_key_event(&event).is_none());
}

#[test]
#[cfg(windows)]
fn terminal_input_modifier_none() {
    assert!(terminal_input_modifier_parameter(false, false, false).is_none());
}

#[test]
#[cfg(windows)]
fn terminal_input_modifier_shift() {
    assert_eq!(
        terminal_input_modifier_parameter(true, false, false),
        Some(2)
    );
}

#[test]
#[cfg(windows)]
fn terminal_input_modifier_alt() {
    assert_eq!(
        terminal_input_modifier_parameter(false, true, false),
        Some(3)
    );
}

#[test]
#[cfg(windows)]
fn terminal_input_modifier_ctrl() {
    assert_eq!(
        terminal_input_modifier_parameter(false, false, true),
        Some(5)
    );
}

#[test]
#[cfg(windows)]
fn terminal_input_modifier_shift_ctrl() {
    assert_eq!(
        terminal_input_modifier_parameter(true, false, true),
        Some(6)
    );
}

#[test]
#[cfg(windows)]
fn control_character_for_unicode_letters() {
    assert_eq!(control_character_for_unicode('A' as u16), Some(0x01));
    assert_eq!(control_character_for_unicode('C' as u16), Some(0x03));
    assert_eq!(control_character_for_unicode('Z' as u16), Some(0x1A));
}

#[test]
#[cfg(windows)]
fn control_character_for_unicode_special() {
    assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
    assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
}

#[test]
#[cfg(windows)]
fn control_character_for_unicode_digit_returns_none() {
    assert!(control_character_for_unicode('1' as u16).is_none());
}

#[test]
#[cfg(windows)]
fn format_terminal_input_bytes_empty() {
    assert_eq!(format_terminal_input_bytes(b""), "[]");
}

#[test]
#[cfg(windows)]
fn format_terminal_input_bytes_multi() {
    assert_eq!(format_terminal_input_bytes(&[0x41, 0x42]), "[41 42]");
}

#[test]
#[cfg(windows)]
fn repeated_tilde_sequence_no_modifier() {
    assert_eq!(repeated_tilde_sequence(3, None, 1), b"\x1b[3~");
}

#[test]
#[cfg(windows)]
fn repeated_tilde_sequence_with_modifier() {
    assert_eq!(repeated_tilde_sequence(3, Some(2), 1), b"\x1b[3;2~");
}

#[test]
#[cfg(windows)]
fn repeated_tilde_sequence_repeated() {
    let result = repeated_tilde_sequence(3, None, 3);
    assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
}

#[test]
#[cfg(windows)]
fn repeated_modified_sequence_no_modifier() {
    let result = repeated_modified_sequence(b"\x1b[A", None, 1);
    assert_eq!(result, b"\x1b[A");
}

#[test]
#[cfg(windows)]
fn repeated_modified_sequence_with_modifier() {
    // Shift modifier (2) applied to Up arrow
    let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
    assert_eq!(result, b"\x1b[1;2A");
}

#[test]
#[cfg(windows)]
fn repeated_modified_sequence_repeated() {
    let result = repeated_modified_sequence(b"\x1b[A", None, 2);
    assert_eq!(result, b"\x1b[A\x1b[A");
}

#[test]
#[cfg(windows)]
fn repeat_terminal_input_bytes_single() {
    let result = repeat_terminal_input_bytes(b"\r", 1);
    assert_eq!(result, b"\r");
}

#[test]
#[cfg(windows)]
fn repeat_terminal_input_bytes_multiple() {
    let result = repeat_terminal_input_bytes(b"ab", 3);
    assert_eq!(result, b"ababab");
}

#[test]
#[cfg(windows)]
fn repeat_terminal_input_bytes_zero_clamps_to_one() {
    let result = repeat_terminal_input_bytes(b"x", 0);
    assert_eq!(result, b"x");
}

// ── B1: Windows Console Key Translation (navigation keys) ──

#[test]
#[cfg(windows)]
fn translate_console_key_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, 0, 1))
        .expect("VK_HOME should translate");
    assert_eq!(event.data, b"\x1b[H");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_end() {
    use winapi::um::winuser::VK_END;
    let event = translate_console_key_event(&key_event(VK_END as u16, 0, 0, 1))
        .expect("VK_END should translate");
    assert_eq!(event.data, b"\x1b[F");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_insert() {
    use winapi::um::winuser::VK_INSERT;
    let event = translate_console_key_event(&key_event(VK_INSERT as u16, 0, 0, 1))
        .expect("VK_INSERT should translate");
    assert_eq!(event.data, b"\x1b[2~");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_delete() {
    use winapi::um::winuser::VK_DELETE;
    let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, 0, 1))
        .expect("VK_DELETE should translate");
    assert_eq!(event.data, b"\x1b[3~");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_page_up() {
    use winapi::um::winuser::VK_PRIOR;
    let event = translate_console_key_event(&key_event(VK_PRIOR as u16, 0, 0, 1))
        .expect("VK_PRIOR should translate");
    assert_eq!(event.data, b"\x1b[5~");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_page_down() {
    use winapi::um::winuser::VK_NEXT;
    let event = translate_console_key_event(&key_event(VK_NEXT as u16, 0, 0, 1))
        .expect("VK_NEXT should translate");
    assert_eq!(event.data, b"\x1b[6~");
    assert!(!event.submit);
}

#[test]
#[cfg(windows)]
fn translate_console_key_shift_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+Home should translate");
    assert_eq!(event.data, b"\x1b[1;2H");
    assert!(event.shift);
}

#[test]
#[cfg(windows)]
fn translate_console_key_shift_end() {
    use winapi::um::winuser::VK_END;
    let event = translate_console_key_event(&key_event(VK_END as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+End should translate");
    assert_eq!(event.data, b"\x1b[1;2F");
    assert!(event.shift);
}

#[test]
#[cfg(windows)]
fn translate_console_key_ctrl_home() {
    use winapi::um::winuser::VK_HOME;
    let event = translate_console_key_event(&key_event(VK_HOME as u16, 0, LEFT_CTRL_PRESSED, 1))
        .expect("Ctrl+Home should translate");
    assert_eq!(event.data, b"\x1b[1;5H");
    assert!(event.ctrl);
}

#[test]
#[cfg(windows)]
fn translate_console_key_shift_delete() {
    use winapi::um::winuser::VK_DELETE;
    let event = translate_console_key_event(&key_event(VK_DELETE as u16, 0, SHIFT_PRESSED, 1))
        .expect("Shift+Delete should translate");
    assert_eq!(event.data, b"\x1b[3;2~");
    assert!(event.shift);
}

#[test]
#[cfg(windows)]
fn translate_console_key_ctrl_page_up() {
    use winapi::um::winuser::VK_PRIOR;
    let event = translate_console_key_event(&key_event(VK_PRIOR as u16, 0, LEFT_CTRL_PRESSED, 1))
        .expect("Ctrl+PageUp should translate");
    assert_eq!(event.data, b"\x1b[5;5~");
    assert!(event.ctrl);
}

#[test]
#[cfg(windows)]
fn translate_console_key_backspace() {
    use winapi::um::winuser::VK_BACK;
    let event = translate_console_key_event(&key_event(VK_BACK as u16, 0x08, 0, 1))
        .expect("Backspace should translate");
    assert_eq!(event.data, b"\x08");
}

#[test]
#[cfg(windows)]
fn translate_console_key_escape() {
    use winapi::um::winuser::VK_ESCAPE;
    let event = translate_console_key_event(&key_event(VK_ESCAPE as u16, 0x1b, 0, 1))
        .expect("Escape should translate");
    assert_eq!(event.data, b"\x1b");
}

#[test]
#[cfg(windows)]
fn translate_console_key_tab() {
    let event = translate_console_key_event(&key_event(VK_TAB as u16, 0, 0, 1))
        .expect("Tab should translate");
    assert_eq!(event.data, b"\t");
}

#[test]
#[cfg(windows)]
fn translate_console_key_plain_enter_is_submit() {
    let event = translate_console_key_event(&key_event(VK_RETURN as u16, '\r' as u16, 0, 1))
        .expect("Enter should translate");
    assert_eq!(event.data, b"\r");
    assert!(event.submit);
    assert!(!event.shift);
}

#[test]
#[cfg(windows)]
fn translate_console_key_unicode_printable() {
    // Regular 'a' key
    let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 1))
        .expect("printable should translate");
    assert_eq!(event.data, b"a");
}

#[test]
#[cfg(windows)]
fn translate_console_key_unicode_repeated() {
    let event = translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, 0, 3))
        .expect("repeated printable should translate");
    assert_eq!(event.data, b"aaa");
}

#[test]
#[cfg(windows)]
fn translate_console_key_down_arrow() {
    use winapi::um::winuser::VK_DOWN;
    let event = translate_console_key_event(&key_event(VK_DOWN as u16, 0, 0, 1))
        .expect("Down arrow should translate");
    assert_eq!(event.data, b"\x1b[B");
}

#[test]
#[cfg(windows)]
fn translate_console_key_right_arrow() {
    use winapi::um::winuser::VK_RIGHT;
    let event = translate_console_key_event(&key_event(VK_RIGHT as u16, 0, 0, 1))
        .expect("Right arrow should translate");
    assert_eq!(event.data, b"\x1b[C");
}

#[test]
#[cfg(windows)]
fn translate_console_key_left_arrow() {
    use winapi::um::winuser::VK_LEFT;
    let event = translate_console_key_event(&key_event(VK_LEFT as u16, 0, 0, 1))
        .expect("Left arrow should translate");
    assert_eq!(event.data, b"\x1b[D");
}

#[test]
#[cfg(windows)]
fn translate_console_key_unknown_vk_no_unicode_returns_none() {
    // Unknown VK with no unicode char → should return None
    let result = translate_console_key_event(&key_event(0xFF, 0, 0, 1));
    assert!(result.is_none());
}

#[test]
#[cfg(windows)]
fn translate_console_key_alt_escape_prefix() {
    // Alt+letter should prepend ESC byte to the character
    let event =
        translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1))
            .expect("Alt+a should translate");
    assert_eq!(event.data, b"\x1ba");
    assert!(event.alt);
}

#[test]
#[cfg(windows)]
fn translate_console_key_ctrl_a() {
    let event =
        translate_console_key_event(&key_event(b'A' as u16, 'a' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("Ctrl+A should translate");
    assert_eq!(event.data, [0x01]); // SOH
    assert!(event.ctrl);
}

#[test]
#[cfg(windows)]
fn translate_console_key_ctrl_z() {
    let event =
        translate_console_key_event(&key_event(b'Z' as u16, 'z' as u16, LEFT_CTRL_PRESSED, 1))
            .expect("Ctrl+Z should translate");
    assert_eq!(event.data, [0x1A]); // SUB
    assert!(event.ctrl);
}

// ── NativeTerminalInput tests ──

#[test]
fn terminal_input_new_starts_closed() {
    let input = NativeTerminalInput::new();
    assert!(!input.capturing());
    let state = input.inner.state.lock().unwrap();
    assert!(state.closed);
    assert!(state.events.is_empty());
}

#[test]
fn terminal_input_available_false_when_empty() {
    let input = NativeTerminalInput::new();
    assert!(!input.available());
}

#[test]
fn terminal_input_next_event_none_when_empty() {
    let input = NativeTerminalInput::new();
    assert!(input.inner.next_event().is_none());
}

#[test]
fn terminal_input_inject_and_consume_event() {
    let input = NativeTerminalInput::new();
    {
        let mut state = input.inner.state.lock().unwrap();
        state.events.push_back(TerminalInputEventRecord {
            data: b"test".to_vec(),
            submit: false,
            shift: false,
            ctrl: false,
            alt: false,
            virtual_key_code: 0,
            repeat_count: 1,
        });
    }
    assert!(input.available());
    let event = input.inner.next_event().unwrap();
    assert_eq!(event.data, b"test");
    assert!(!input.available());
}

#[test]
#[cfg(not(windows))]
fn terminal_input_start_errors_on_non_windows() {
    pyo3::Python::initialize();
    let input = NativeTerminalInput::new();
    let result = input.start();
    assert!(result.is_err());
}

// ── NativeTerminalInputEvent __repr__ ──

#[test]
fn terminal_input_event_repr() {
    let event = NativeTerminalInputEvent {
        data: vec![0x0D],
        submit: true,
        shift: false,
        ctrl: false,
        alt: false,
        virtual_key_code: 13,
        repeat_count: 1,
    };
    let repr = event.__repr__();
    assert!(repr.contains("submit=true"));
    assert!(repr.contains("virtual_key_code=13"));
}

// ── NativeTerminalInput additional tests ──

#[test]
fn terminal_input_inject_multiple_events() {
    let input = NativeTerminalInput::new();
    {
        let mut state = input.inner.state.lock().unwrap();
        for i in 0..5 {
            state.events.push_back(TerminalInputEventRecord {
                data: vec![b'a' + i],
                submit: false,
                shift: false,
                ctrl: false,
                alt: false,
                virtual_key_code: 0,
                repeat_count: 1,
            });
        }
    }
    assert!(input.available());
    let mut count = 0;
    while input.inner.next_event().is_some() {
        count += 1;
    }
    assert_eq!(count, 5);
    assert!(!input.available());
}

#[test]
fn terminal_input_capturing_false_initially() {
    let input = NativeTerminalInput::new();
    assert!(!input.capturing());
}

// ── NativeTerminalInputEvent fields ──

#[test]
fn terminal_input_event_fields() {
    let event = NativeTerminalInputEvent {
        data: vec![0x1B, 0x5B, 0x41],
        submit: false,
        shift: true,
        ctrl: true,
        alt: false,
        virtual_key_code: 38,
        repeat_count: 2,
    };
    assert_eq!(event.data, vec![0x1B, 0x5B, 0x41]);
    assert!(!event.submit);
    assert!(event.shift);
    assert!(event.ctrl);
    assert!(!event.alt);
    assert_eq!(event.virtual_key_code, 38);
    assert_eq!(event.repeat_count, 2);
    // __repr__ should include all flags
    let repr = event.__repr__();
    assert!(repr.contains("shift=true"));
    assert!(repr.contains("ctrl=true"));
    assert!(repr.contains("alt=false"));
}

// ── Windows additional tests (from iter2) ──

#[cfg(windows)]
mod windows_additional_tests {
    use super::*;
    use winapi::um::winuser::VK_F1;

    // ── control_character_for_unicode tests ──

    #[test]
    fn control_char_at_sign() {
        assert_eq!(control_character_for_unicode('@' as u16), Some(0x00));
    }

    #[test]
    fn control_char_space() {
        assert_eq!(control_character_for_unicode(' ' as u16), Some(0x00));
    }

    #[test]
    fn control_char_a() {
        assert_eq!(control_character_for_unicode('a' as u16), Some(0x01));
    }

    #[test]
    fn control_char_z() {
        assert_eq!(control_character_for_unicode('z' as u16), Some(0x1A));
    }

    #[test]
    fn control_char_bracket() {
        assert_eq!(control_character_for_unicode('[' as u16), Some(0x1B));
    }

    #[test]
    fn control_char_backslash() {
        assert_eq!(control_character_for_unicode('\\' as u16), Some(0x1C));
    }

    #[test]
    fn control_char_close_bracket() {
        assert_eq!(control_character_for_unicode(']' as u16), Some(0x1D));
    }

    #[test]
    fn control_char_caret() {
        assert_eq!(control_character_for_unicode('^' as u16), Some(0x1E));
    }

    #[test]
    fn control_char_underscore() {
        assert_eq!(control_character_for_unicode('_' as u16), Some(0x1F));
    }

    #[test]
    fn control_char_digit_returns_none() {
        assert_eq!(control_character_for_unicode('0' as u16), None);
    }

    #[test]
    fn control_char_exclamation_returns_none() {
        assert_eq!(control_character_for_unicode('!' as u16), None);
    }

    // ── terminal_input_modifier_parameter tests ──

    #[test]
    fn modifier_param_no_modifiers_returns_none() {
        assert_eq!(terminal_input_modifier_parameter(false, false, false), None);
    }

    #[test]
    fn modifier_param_shift_only() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, false),
            Some(2)
        );
    }

    #[test]
    fn modifier_param_alt_only() {
        assert_eq!(
            terminal_input_modifier_parameter(false, true, false),
            Some(3)
        );
    }

    #[test]
    fn modifier_param_ctrl_only() {
        assert_eq!(
            terminal_input_modifier_parameter(false, false, true),
            Some(5)
        );
    }

    #[test]
    fn modifier_param_shift_ctrl() {
        assert_eq!(
            terminal_input_modifier_parameter(true, false, true),
            Some(6)
        );
    }

    #[test]
    fn modifier_param_shift_alt() {
        assert_eq!(
            terminal_input_modifier_parameter(true, true, false),
            Some(4)
        );
    }

    #[test]
    fn modifier_param_all_modifiers() {
        assert_eq!(terminal_input_modifier_parameter(true, true, true), Some(8));
    }

    // ── repeated_tilde_sequence tests ──

    #[test]
    fn tilde_sequence_no_modifier() {
        let result = repeated_tilde_sequence(3, None, 1);
        assert_eq!(result, b"\x1b[3~");
    }

    #[test]
    fn tilde_sequence_with_modifier() {
        let result = repeated_tilde_sequence(3, Some(2), 1);
        assert_eq!(result, b"\x1b[3;2~");
    }

    #[test]
    fn tilde_sequence_repeated() {
        let result = repeated_tilde_sequence(3, None, 3);
        assert_eq!(result, b"\x1b[3~\x1b[3~\x1b[3~");
    }

    // ── repeated_modified_sequence tests ──

    #[test]
    fn modified_sequence_no_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", None, 1);
        assert_eq!(result, b"\x1b[A");
    }

    #[test]
    fn modified_sequence_with_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", Some(2), 1);
        assert_eq!(result, b"\x1b[1;2A");
    }

    #[test]
    fn modified_sequence_repeated_with_modifier() {
        let result = repeated_modified_sequence(b"\x1b[A", Some(5), 2);
        assert_eq!(result, b"\x1b[1;5A\x1b[1;5A");
    }

    // ── format_terminal_input_bytes tests ──

    #[test]
    fn format_bytes_empty() {
        assert_eq!(format_terminal_input_bytes(&[]), "[]");
    }

    #[test]
    fn format_bytes_multiple() {
        assert_eq!(
            format_terminal_input_bytes(&[0x1B, 0x5B, 0x41]),
            "[1b 5b 41]"
        );
    }

    // ── native_terminal_input_trace_target tests ──

    #[test]
    fn trace_target_empty_env_returns_none() {
        with_locked_env_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, None, || {
            assert!(native_terminal_input_trace_target().is_none());
        });
    }

    #[test]
    fn trace_target_whitespace_env_returns_none() {
        with_locked_env_var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV, Some("   "), || {
            assert!(native_terminal_input_trace_target().is_none());
        });
    }

    #[test]
    fn trace_target_valid_env_returns_value() {
        with_locked_env_var(
            NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV,
            Some("/tmp/trace.log"),
            || {
                let result = native_terminal_input_trace_target();
                assert_eq!(result, Some("/tmp/trace.log".to_string()));
            },
        );
    }

    // ── translate_console_key_event: key-up ignored ──

    #[test]
    fn translate_key_up_event_returns_none() {
        let mut event: KEY_EVENT_RECORD = unsafe { std::mem::zeroed() };
        event.bKeyDown = 0;
        event.wVirtualKeyCode = VK_RETURN as u16;
        let result = translate_console_key_event(&event);
        assert!(result.is_none());
    }

    // ── translate: F1 returns None (unknown key) ──

    #[test]
    fn translate_f1_key_returns_none() {
        let event = key_event(VK_F1 as u16, 0, 0, 1);
        let result = translate_console_key_event(&event);
        assert!(result.is_none());
    }

    // ── translate: alt prefix ──

    #[test]
    fn translate_alt_a_has_escape_prefix() {
        let event = key_event('a' as u16, 'a' as u16, LEFT_ALT_PRESSED, 1);
        let result = translate_console_key_event(&event).unwrap();
        assert!(result.data.starts_with(b"\x1b"));
        assert!(result.alt);
    }

    // ── translate: Ctrl+character ──

    #[test]
    fn translate_ctrl_c_produces_etx() {
        let event = key_event('C' as u16, 'c' as u16, LEFT_CTRL_PRESSED, 1);
        let result = translate_console_key_event(&event).unwrap();
        assert_eq!(result.data, &[0x03]);
        assert!(result.ctrl);
    }
}
