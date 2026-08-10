use crate::pty_buffer::NativePtyBuffer;

// ── NativePtyBuffer tests (non-Python methods) ──

#[test]
fn pty_buffer_available_empty() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    assert!(!buf.available());
}

#[test]
fn pty_buffer_record_and_available() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"hello");
    assert!(buf.available());
}

#[test]
fn pty_buffer_history_bytes_and_clear() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"hello");
    buf.record_output(b"world");
    assert_eq!(buf.history_bytes(), 10);
    let released = buf.clear_history();
    assert_eq!(released, 10);
    assert_eq!(buf.history_bytes(), 0);
}

#[test]
fn pty_buffer_close() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.close();
    // After close, buffer is marked as closed
    // (no panic, graceful handling)
}

// ── NativePtyBuffer tests with PyO3 ──

#[test]
fn pty_buffer_drain_returns_recorded_chunks() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"chunk1");
        buf.record_output(b"chunk2");
        let drained = buf.drain(py).unwrap();
        assert_eq!(drained.len(), 2);
        assert!(!buf.available());
    });
}

#[test]
fn pty_buffer_output_returns_full_history() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(true, "utf-8", "replace");
        buf.record_output(b"hello ");
        buf.record_output(b"world");
        let output = buf.output(py).unwrap();
        let text: String = output.extract(py).unwrap();
        assert_eq!(text, "hello world");
    });
}

#[test]
fn pty_buffer_output_since_offset() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(true, "utf-8", "replace");
        buf.record_output(b"hello ");
        buf.record_output(b"world");
        let output = buf.output_since(py, 6).unwrap();
        let text: String = output.extract(py).unwrap();
        assert_eq!(text, "world");
    });
}

#[test]
fn pty_buffer_read_non_blocking_empty() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        let result = buf.read_non_blocking(py).unwrap();
        assert!(result.is_none());
    });
}

#[test]
fn pty_buffer_read_non_blocking_with_data() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"data");
        let result = buf.read_non_blocking(py).unwrap();
        assert!(result.is_some());
    });
}

#[test]
fn pty_buffer_read_closed_returns_error() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.close();
        let result = buf.read_non_blocking(py);
        assert!(result.is_err());
    });
}

#[test]
fn pty_buffer_read_with_timeout() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        let result = buf.read(py, Some(0.05));
        // Should timeout since no data
        assert!(result.is_err());
    });
}

#[test]
fn pty_buffer_text_mode_decodes_utf8() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(true, "utf-8", "replace");
        buf.record_output("héllo".as_bytes());
        let result = buf.read_non_blocking(py).unwrap().unwrap();
        let text: String = result.extract(py).unwrap();
        assert_eq!(text, "héllo");
    });
}

#[test]
fn pty_buffer_bytes_mode_returns_bytes() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"\xff\xfe");
        let result = buf.read_non_blocking(py).unwrap().unwrap();
        let bytes: Vec<u8> = result.extract(py).unwrap();
        assert_eq!(bytes, vec![0xff, 0xfe]);
    });
}

// ── NativePtyBuffer additional tests ──

#[test]
fn pty_buffer_multiple_record_and_drain() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        buf.record_output(b"a");
        buf.record_output(b"b");
        buf.record_output(b"c");
        let drained = buf.drain(py).unwrap();
        assert_eq!(drained.len(), 3);
        assert!(!buf.available());
        // history should still be available
        assert_eq!(buf.history_bytes(), 3);
    });
}

#[test]
fn pty_buffer_output_since_beyond_length() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(true, "utf-8", "replace");
        buf.record_output(b"hi");
        let output = buf.output_since(py, 999).unwrap();
        let text: String = output.extract(py).unwrap();
        assert_eq!(text, "");
    });
}

#[test]
fn pty_buffer_clear_history_returns_correct_bytes() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"hello");
    buf.record_output(b"world");
    assert_eq!(buf.history_bytes(), 10);
    let released = buf.clear_history();
    assert_eq!(released, 10);
    assert_eq!(buf.history_bytes(), 0);
    // Record more after clear
    buf.record_output(b"new");
    assert_eq!(buf.history_bytes(), 3);
}

// ── Iteration 3: NativePtyBuffer additional tests ──

#[test]
fn pty_buffer_new_defaults() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    assert!(!buf.available());
    assert_eq!(buf.history_bytes(), 0);
}

#[test]
fn pty_buffer_record_output_makes_available() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"hello");
    assert!(buf.available());
}

#[test]
fn pty_buffer_history_bytes_accumulates() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"hello");
    assert_eq!(buf.history_bytes(), 5);
    buf.record_output(b" world");
    assert_eq!(buf.history_bytes(), 11);
}

#[test]
fn pty_buffer_clear_history_resets_to_zero() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"data");
    let released = buf.clear_history();
    assert_eq!(released, 4);
    assert_eq!(buf.history_bytes(), 0);
}

#[test]
fn pty_buffer_close_sets_closed_flag() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.close();
    let state = buf.state.lock().unwrap();
    assert!(state.closed);
}

#[test]
fn pty_buffer_record_multiple_chunks_all_available() {
    let buf = NativePtyBuffer::new(false, "utf-8", "replace");
    buf.record_output(b"a");
    buf.record_output(b"bb");
    buf.record_output(b"ccc");
    assert_eq!(buf.history_bytes(), 6);
    let state = buf.state.lock().unwrap();
    assert_eq!(state.chunks.len(), 3);
}

// ── NativePtyBuffer decode_chunk tests ──

#[test]
fn pty_buffer_decode_chunk_text_mode() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(true, "utf-8", "replace");
        let result = buf.decode_chunk(py, b"hello").unwrap();
        let text: String = result.extract(py).unwrap();
        assert_eq!(text, "hello");
    });
}

#[test]
fn pty_buffer_decode_chunk_binary_mode() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let buf = NativePtyBuffer::new(false, "utf-8", "replace");
        let result = buf.decode_chunk(py, b"\xff\xfe").unwrap();
        let bytes: Vec<u8> = result.extract(py).unwrap();
        assert_eq!(bytes, vec![0xff, 0xfe]);
    });
}
