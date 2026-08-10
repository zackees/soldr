use std::collections::VecDeque;
#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Environment variable name for the trace file path.
pub const NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV: &str =
    "RUNNING_PROCESS_NATIVE_TERMINAL_INPUT_TRACE_PATH";

// ── Error type ──

/// Errors returned by native terminal input capture.
#[derive(Debug, Error)]
pub enum TerminalInputError {
    /// Terminal input capture has already closed.
    #[error("terminal input is closed")]
    Closed,
    /// No terminal input arrived before the requested timeout.
    #[error("no terminal input available before timeout")]
    Timeout,
    /// An operating-system I/O operation failed.
    #[error("terminal input I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A terminal input error that does not fit a narrower variant.
    #[error("terminal input error: {0}")]
    Other(String),
}

// ── Pure-Rust data types ──

/// A translated terminal input event ready to forward to a PTY.
#[derive(Clone)]
pub struct TerminalInputEventRecord {
    /// Bytes to write to the PTY for this event.
    pub data: Vec<u8>,
    /// Whether this event represents an unmodified Enter submit action.
    pub submit: bool,
    /// Whether Shift was active when the event was captured.
    pub shift: bool,
    /// Whether Ctrl was active when the event was captured.
    pub ctrl: bool,
    /// Whether Alt was active when the event was captured.
    pub alt: bool,
    /// Virtual-key code for the source key event.
    pub virtual_key_code: u16,
    /// Number of key repeats represented by this event.
    pub repeat_count: u16,
}

/// Shared queue state for captured terminal input events.
pub struct TerminalInputState {
    /// Queued translated terminal input events.
    pub events: VecDeque<TerminalInputEventRecord>,
    /// Whether the capture stream has been closed.
    pub closed: bool,
}

#[cfg(windows)]
/// Windows console state saved while native input capture is active.
pub struct ActiveTerminalInputCapture {
    /// Raw Windows console input handle as an integer.
    pub input_handle: usize,
    /// Console mode to restore when capture stops.
    pub original_mode: u32,
    /// Console mode installed while capture is active.
    pub active_mode: u32,
}

#[cfg(windows)]
/// Result of waiting for a Windows terminal input event.
#[derive(Debug, PartialEq)]
pub enum TerminalInputWaitOutcome {
    /// A translated input event was received.
    Event(TerminalInputEventRecord),
    /// Terminal input capture closed before an event arrived.
    Closed,
    /// No event arrived before the timeout.
    Timeout,
}

#[cfg(windows)]
impl std::fmt::Debug for TerminalInputEventRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalInputEventRecord")
            .field("data", &self.data)
            .field("submit", &self.submit)
            .field("shift", &self.shift)
            .field("ctrl", &self.ctrl)
            .field("alt", &self.alt)
            .field("virtual_key_code", &self.virtual_key_code)
            .field("repeat_count", &self.repeat_count)
            .finish()
    }
}

#[cfg(windows)]
impl PartialEq for TerminalInputEventRecord {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
            && self.submit == other.submit
            && self.shift == other.shift
            && self.ctrl == other.ctrl
            && self.alt == other.alt
            && self.virtual_key_code == other.virtual_key_code
            && self.repeat_count == other.repeat_count
    }
}

// ── Utility functions ──

/// Returns the current Unix timestamp as fractional seconds.
pub fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(windows)]
/// Returns the configured native input trace target, if tracing is enabled.
pub fn native_terminal_input_trace_target() -> Option<String> {
    std::env::var(NATIVE_TERMINAL_INPUT_TRACE_PATH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
/// Appends one native input trace line to the configured target.
pub fn append_native_terminal_input_trace_line(line: &str) {
    let Some(target) = native_terminal_input_trace_target() else {
        return;
    };
    if target == "-" {
        eprintln!("{line}");
        return;
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&target) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

#[cfg(windows)]
/// Formats bytes as lowercase hexadecimal values for trace output.
pub fn format_terminal_input_bytes(data: &[u8]) -> String {
    if data.is_empty() {
        return "[]".to_string();
    }
    let parts: Vec<String> = data.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("[{}]", parts.join(" "))
}

// ── Console mode / key translation helpers ──

#[cfg(windows)]
/// Builds the Windows console mode used during native input capture.
pub fn native_terminal_input_mode(original_mode: u32) -> u32 {
    use winapi::um::wincon::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_WINDOW_INPUT,
    };

    (original_mode | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT)
        & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE)
}

#[cfg(windows)]
/// Returns the VT modifier parameter for the active modifier keys.
pub fn terminal_input_modifier_parameter(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + (u8::from(alt) * 2) + (u8::from(ctrl) * 4);
    (value > 1).then_some(value)
}

#[cfg(windows)]
/// Repeats translated bytes according to a Windows key repeat count.
pub fn repeat_terminal_input_bytes(chunk: &[u8], repeat_count: u16) -> Vec<u8> {
    let repeat = usize::from(repeat_count.max(1));
    let mut output = Vec::with_capacity(chunk.len() * repeat);
    for _ in 0..repeat {
        output.extend_from_slice(chunk);
    }
    output
}

#[cfg(windows)]
/// Adds a VT CSI modifier parameter to a base sequence when needed and repeats it.
pub fn repeated_modified_sequence(base: &[u8], modifier: Option<u8>, repeat_count: u16) -> Vec<u8> {
    if let Some(value) = modifier {
        let base_text = std::str::from_utf8(base).expect("VT sequence literal must be utf-8");
        let body = base_text
            .strip_prefix("\x1b[")
            .expect("VT sequence literal must start with CSI");
        let sequence = format!("\x1b[1;{value}{body}");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    } else {
        repeat_terminal_input_bytes(base, repeat_count)
    }
}

#[cfg(windows)]
/// Builds and repeats a VT CSI tilde sequence with an optional modifier.
pub fn repeated_tilde_sequence(number: u8, modifier: Option<u8>, repeat_count: u16) -> Vec<u8> {
    if let Some(value) = modifier {
        let sequence = format!("\x1b[{number};{value}~");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    } else {
        let sequence = format!("\x1b[{number}~");
        repeat_terminal_input_bytes(sequence.as_bytes(), repeat_count)
    }
}

#[cfg(windows)]
/// Maps a Unicode code unit to the corresponding Ctrl-key control byte.
pub fn control_character_for_unicode(unicode: u16) -> Option<u8> {
    let upper = char::from_u32(u32::from(unicode))?.to_ascii_uppercase();
    match upper {
        '@' | ' ' => Some(0x00),
        'A'..='Z' => Some((upper as u8) - b'@'),
        '[' => Some(0x1B),
        '\\' => Some(0x1C),
        ']' => Some(0x1D),
        '^' => Some(0x1E),
        '_' => Some(0x1F),
        _ => None,
    }
}

#[cfg(windows)]
/// Writes a trace record for a translated key event and returns the event unchanged.
pub fn trace_translated_console_key_event(
    record: &winapi::um::wincontypes::KEY_EVENT_RECORD,
    event: TerminalInputEventRecord,
) -> TerminalInputEventRecord {
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input raw bKeyDown={} vk={:#06x} scan={:#06x} unicode={:#06x} control={:#010x} repeat={} translated bytes={} submit={} shift={} ctrl={} alt={}",
        unix_now_seconds(),
        record.bKeyDown,
        record.wVirtualKeyCode,
        record.wVirtualScanCode,
        unsafe { *record.uChar.UnicodeChar() },
        record.dwControlKeyState,
        record.wRepeatCount.max(1),
        format_terminal_input_bytes(&event.data),
        event.submit,
        event.shift,
        event.ctrl,
        event.alt,
    ));
    event
}

#[cfg(windows)]
/// Translates a Windows console key event into PTY input bytes.
pub fn translate_console_key_event(
    record: &winapi::um::wincontypes::KEY_EVENT_RECORD,
) -> Option<TerminalInputEventRecord> {
    use winapi::um::wincontypes::{
        LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
    };
    use winapi::um::winuser::{
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT,
        VK_PRIOR, VK_RETURN, VK_RIGHT, VK_TAB, VK_UP,
    };

    if record.bKeyDown == 0 {
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input raw bKeyDown=0 vk={:#06x} scan={:#06x} unicode={:#06x} control={:#010x} repeat={} translated=ignored",
            unix_now_seconds(),
            record.wVirtualKeyCode,
            record.wVirtualScanCode,
            unsafe { *record.uChar.UnicodeChar() },
            record.dwControlKeyState,
            record.wRepeatCount,
        ));
        return None;
    }

    let repeat_count = record.wRepeatCount.max(1);
    let modifiers = record.dwControlKeyState;
    let shift = modifiers & SHIFT_PRESSED != 0;
    let alt = modifiers & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0;
    let ctrl = modifiers & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
    let virtual_key_code = record.wVirtualKeyCode;
    let unicode = unsafe { *record.uChar.UnicodeChar() };

    // Shift+Enter: send CSI u escape sequence so downstream TUI apps
    // (e.g. Claude Code) can distinguish Shift+Enter (newline) from
    // plain Enter (submit).  Format: ESC [ 13 ; 2 u
    if shift && !ctrl && !alt && virtual_key_code as i32 == VK_RETURN {
        return Some(trace_translated_console_key_event(
            record,
            TerminalInputEventRecord {
                data: repeat_terminal_input_bytes(b"\x1b[13;2u", repeat_count),
                submit: false,
                shift,
                ctrl,
                alt,
                virtual_key_code,
                repeat_count,
            },
        ));
    }

    let mut data = if ctrl {
        control_character_for_unicode(unicode)
            .map(|byte| repeat_terminal_input_bytes(&[byte], repeat_count))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    if data.is_empty() && unicode != 0 {
        if let Some(character) = char::from_u32(u32::from(unicode)) {
            let text: String = std::iter::repeat_n(character, usize::from(repeat_count)).collect();
            data = text.into_bytes();
        }
    }

    if data.is_empty() {
        let modifier = terminal_input_modifier_parameter(shift, alt, ctrl);
        let sequence = match virtual_key_code as i32 {
            VK_BACK => Some(b"\x08".as_slice()),
            VK_TAB if shift => Some(b"\x1b[Z".as_slice()),
            VK_TAB => Some(b"\t".as_slice()),
            VK_RETURN => Some(b"\r".as_slice()),
            VK_ESCAPE => Some(b"\x1b".as_slice()),
            VK_UP => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[A", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_DOWN => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[B", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_RIGHT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[C", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_LEFT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[D", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_HOME => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[H", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_END => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_modified_sequence(b"\x1b[F", modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_INSERT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(2, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_DELETE => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(3, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_PRIOR => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(5, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            VK_NEXT => {
                return Some(trace_translated_console_key_event(
                    record,
                    TerminalInputEventRecord {
                        data: repeated_tilde_sequence(6, modifier, repeat_count),
                        submit: false,
                        shift,
                        ctrl,
                        alt,
                        virtual_key_code,
                        repeat_count,
                    },
                ));
            }
            _ => None,
        };
        data = sequence.map(|chunk| repeat_terminal_input_bytes(chunk, repeat_count))?;
    }

    if alt && !data.starts_with(b"\x1b[") && !data.starts_with(b"\x1bO") {
        let mut prefixed = Vec::with_capacity(data.len() + 1);
        prefixed.push(0x1B);
        prefixed.extend_from_slice(&data);
        data = prefixed;
    }

    let event = TerminalInputEventRecord {
        data,
        submit: virtual_key_code as i32 == VK_RETURN && !shift,
        shift,
        ctrl,
        alt,
        virtual_key_code,
        repeat_count,
    };
    Some(trace_translated_console_key_event(record, event))
}

// ── Worker thread ──

#[cfg(windows)]
/// Runs the Windows console input worker that queues translated key events.
pub fn native_terminal_input_worker(
    input_handle: usize,
    state: Arc<Mutex<TerminalInputState>>,
    condvar: Arc<Condvar>,
    stop: Arc<AtomicBool>,
    capturing: Arc<AtomicBool>,
) {
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::winerror::WAIT_TIMEOUT;
    use winapi::um::consoleapi::ReadConsoleInputW;
    use winapi::um::synchapi::WaitForSingleObject;
    use winapi::um::winbase::WAIT_OBJECT_0;
    use winapi::um::wincontypes::{INPUT_RECORD, KEY_EVENT};
    use winapi::um::winnt::HANDLE;

    let handle = input_handle as HANDLE;
    let mut records: [INPUT_RECORD; 512] = unsafe { std::mem::zeroed() };
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input worker_start handle={input_handle}",
        unix_now_seconds(),
    ));

    while !stop.load(Ordering::Acquire) {
        let wait_result = unsafe { WaitForSingleObject(handle, 50) };
        match wait_result {
            WAIT_OBJECT_0 => {
                let mut read_count: DWORD = 0;
                let ok = unsafe {
                    ReadConsoleInputW(
                        handle,
                        records.as_mut_ptr(),
                        records.len() as DWORD,
                        &mut read_count,
                    )
                };
                if ok == 0 {
                    append_native_terminal_input_trace_line(&format!(
                        "[{:.6}] native_terminal_input read_console_input_failed handle={input_handle}",
                        unix_now_seconds(),
                    ));
                    break;
                }
                let mut batch = Vec::new();
                for record in records.iter().take(read_count as usize) {
                    if record.EventType != KEY_EVENT {
                        continue;
                    }
                    let key_event = unsafe { record.Event.KeyEvent() };
                    if let Some(event) = translate_console_key_event(key_event) {
                        batch.push(event);
                    }
                }
                if !batch.is_empty() {
                    let mut guard = state.lock().expect("terminal input mutex poisoned");
                    guard.events.extend(batch);
                    drop(guard);
                    condvar.notify_all();
                }
            }
            WAIT_TIMEOUT => continue,
            _ => {
                append_native_terminal_input_trace_line(&format!(
                    "[{:.6}] native_terminal_input wait_result={wait_result} handle={input_handle}",
                    unix_now_seconds(),
                ));
                break;
            }
        }
    }

    capturing.store(false, Ordering::Release);
    let mut guard = state.lock().expect("terminal input mutex poisoned");
    guard.closed = true;
    condvar.notify_all();
    drop(guard);
    append_native_terminal_input_trace_line(&format!(
        "[{:.6}] native_terminal_input worker_stop handle={input_handle}",
        unix_now_seconds(),
    ));
}

// ── Wait helper ──

#[cfg(windows)]
/// Waits for the next queued terminal input event, closure, or timeout.
pub fn wait_for_terminal_input_event(
    state: &Arc<Mutex<TerminalInputState>>,
    condvar: &Arc<Condvar>,
    timeout: Option<Duration>,
) -> TerminalInputWaitOutcome {
    let deadline = timeout.map(|limit| Instant::now() + limit);
    let mut guard = state.lock().expect("terminal input mutex poisoned");
    loop {
        if let Some(event) = guard.events.pop_front() {
            return TerminalInputWaitOutcome::Event(event);
        }
        if guard.closed {
            return TerminalInputWaitOutcome::Closed;
        }
        match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return TerminalInputWaitOutcome::Timeout;
                }
                let wait = deadline.saturating_duration_since(now);
                let result = condvar
                    .wait_timeout(guard, wait)
                    .expect("terminal input mutex poisoned");
                guard = result.0;
            }
            None => {
                guard = condvar.wait(guard).expect("terminal input mutex poisoned");
            }
        }
    }
}

// ── TerminalInputCore ──

/// Shared native terminal input capture core.
pub struct TerminalInputCore {
    /// Shared queue and closed flag for captured input.
    pub state: Arc<Mutex<TerminalInputState>>,
    /// Condition variable signaled when input state changes.
    pub condvar: Arc<Condvar>,
    /// Stop flag observed by the capture worker.
    pub stop: Arc<AtomicBool>,
    /// Whether native input capture is currently active.
    pub capturing: Arc<AtomicBool>,
    /// Worker thread handle for active capture.
    pub worker: Mutex<Option<thread::JoinHandle<()>>>,
    #[cfg(windows)]
    /// Saved Windows console capture state.
    pub console: Mutex<Option<ActiveTerminalInputCapture>>,
}

impl Default for TerminalInputCore {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalInputCore {
    /// Creates an idle terminal input core with capture stopped.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TerminalInputState {
                events: VecDeque::new(),
                closed: true,
            })),
            condvar: Arc::new(Condvar::new()),
            stop: Arc::new(AtomicBool::new(false)),
            capturing: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            #[cfg(windows)]
            console: Mutex::new(None),
        }
    }

    /// Pops the next queued terminal input event without blocking.
    pub fn next_event(&self) -> Option<TerminalInputEventRecord> {
        self.state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .pop_front()
    }

    /// Returns whether at least one input event is queued.
    pub fn available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .events
            .is_empty()
    }

    /// Returns whether native terminal input capture is active.
    pub fn capturing(&self) -> bool {
        self.capturing.load(Ordering::Acquire)
    }

    /// Returns the saved original Windows console mode, if capture is active.
    pub fn original_console_mode(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            return self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .as_ref()
                .map(|capture| capture.original_mode);
        }

        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Returns the active Windows console mode, if capture is active.
    pub fn active_console_mode(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            return self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .as_ref()
                .map(|capture| capture.active_mode);
        }

        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Blocks until an input event, closure, or optional timeout.
    pub fn wait_for_event(
        &self,
        timeout: Option<f64>,
    ) -> Result<TerminalInputEventRecord, TerminalInputError> {
        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = state.lock().expect("terminal input mutex poisoned");
        loop {
            if let Some(event) = guard.events.pop_front() {
                return Ok(event);
            }
            if guard.closed {
                return Err(TerminalInputError::Closed);
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(TerminalInputError::Timeout);
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = condvar
                        .wait_timeout(guard, wait)
                        .expect("terminal input mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = condvar.wait(guard).expect("terminal input mutex poisoned");
                }
            }
        }
    }

    /// Drains all queued terminal input events.
    pub fn drain_events(&self) -> Vec<TerminalInputEventRecord> {
        let mut guard = self.state.lock().expect("terminal input mutex poisoned");
        guard.events.drain(..).collect()
    }

    /// Stops native terminal input capture and restores console state.
    pub fn stop_impl(&self) -> Result<(), std::io::Error> {
        self.stop.store(true, Ordering::Release);
        #[cfg(windows)]
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input stop_requested",
            unix_now_seconds(),
        ));
        if let Some(worker) = self
            .worker
            .lock()
            .expect("terminal input worker mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
        self.capturing.store(false, Ordering::Release);

        #[cfg(windows)]
        let restore_result = {
            use winapi::um::consoleapi::SetConsoleMode;
            use winapi::um::winnt::HANDLE;

            let console = self
                .console
                .lock()
                .expect("terminal input console mutex poisoned")
                .take();
            console.map(|capture| unsafe {
                SetConsoleMode(capture.input_handle as HANDLE, capture.original_mode)
            })
        };

        let mut guard = self.state.lock().expect("terminal input mutex poisoned");
        guard.closed = true;
        self.condvar.notify_all();
        drop(guard);

        #[cfg(windows)]
        if let Some(result) = restore_result {
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    /// Starts native terminal input capture for the attached Windows console.
    pub fn start_impl(&self) -> Result<(), std::io::Error> {
        use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::processenv::GetStdHandle;
        use winapi::um::winbase::STD_INPUT_HANDLE;

        let mut worker_guard = self
            .worker
            .lock()
            .expect("terminal input worker mutex poisoned");
        if worker_guard.is_some() {
            return Ok(());
        }

        let input_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if input_handle.is_null() || input_handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let mut original_mode = 0u32;
        let got_mode = unsafe { GetConsoleMode(input_handle, &mut original_mode) };
        if got_mode == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "TerminalInputCore requires an attached Windows console stdin",
            ));
        }

        let active_mode = native_terminal_input_mode(original_mode);
        let set_mode = unsafe { SetConsoleMode(input_handle, active_mode) };
        if set_mode == 0 {
            return Err(std::io::Error::last_os_error());
        }
        append_native_terminal_input_trace_line(&format!(
            "[{:.6}] native_terminal_input start handle={} original_mode={:#010x} active_mode={:#010x}",
            unix_now_seconds(),
            input_handle as usize,
            original_mode,
            active_mode,
        ));

        self.stop.store(false, Ordering::Release);
        self.capturing.store(true, Ordering::Release);
        {
            let mut state = self.state.lock().expect("terminal input mutex poisoned");
            state.events.clear();
            state.closed = false;
        }
        *self
            .console
            .lock()
            .expect("terminal input console mutex poisoned") = Some(ActiveTerminalInputCapture {
            input_handle: input_handle as usize,
            original_mode,
            active_mode,
        });

        let state = Arc::clone(&self.state);
        let condvar = Arc::clone(&self.condvar);
        let stop = Arc::clone(&self.stop);
        let capturing = Arc::clone(&self.capturing);
        let input_handle_raw = input_handle as usize;
        *worker_guard = Some(thread::spawn(move || {
            native_terminal_input_worker(input_handle_raw, state, condvar, stop, capturing);
        }));
        Ok(())
    }
}

impl Drop for TerminalInputCore {
    fn drop(&mut self) {
        let _ = self.stop_impl();
    }
}
