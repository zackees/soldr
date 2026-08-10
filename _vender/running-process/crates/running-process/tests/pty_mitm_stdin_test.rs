//! MITM stdin substrate tests for #448.
//!
//! Verifies that bytes the host writes to the PTY's master input
//! pipe reach the child verbatim, with no CR/LF rewriting, control-
//! byte interception, or host-side echo. Each test spawns
//! `testbin-stdin-echoer` and uses byte-exact assertions to catch
//! any substrate translation a future ConPTY swap might introduce.
//!
//! Downstream consumer: `clud`'s Shift+Enter feature, which injects
//! a bare `\n` into the Claude CLI's stdin to distinguish "newline
//! in input" from "submit" (`\r`). Substrate failure here breaks
//! that feature on Windows.

mod common;

use std::time::{Duration, Instant};

use common::mitm_stdin::{skip_unless_mitm_supported, EchoerSession};

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);

macro_rules! skip_if_unsupported {
    () => {
        if skip_unless_mitm_supported() {
            return;
        }
    };
}

// ── 1. Byte-exact transit of single bytes ─────────────────────────

#[test]
fn single_byte_letter_a_round_trips_verbatim() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"a");
    session.assert_received_exact(b"a", RECEIVE_TIMEOUT);
}

#[test]
fn single_byte_carriage_return_round_trips_verbatim() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\r");
    session.assert_received_exact(b"\r", RECEIVE_TIMEOUT);
}

#[test]
fn single_byte_newline_round_trips_verbatim() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\n");
    session.assert_received_exact(b"\n", RECEIVE_TIMEOUT);
}

#[test]
fn ctrl_c_byte_round_trips_verbatim() {
    // 0x03 = ETX = Ctrl+C as a *byte*, not a signal. The host sends
    // it as data; the child must receive it as data because the
    // child decides whether to treat it as a signal (it's outside
    // ConPTY's job to intercept here).
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\x03");
    session.assert_received_exact(b"\x03", RECEIVE_TIMEOUT);
}

#[test]
fn esc_byte_round_trips_verbatim() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\x1b");
    session.assert_received_exact(b"\x1b", RECEIVE_TIMEOUT);
}

#[test]
fn del_byte_round_trips_verbatim() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\x7f");
    session.assert_received_exact(b"\x7f", RECEIVE_TIMEOUT);
}

// ── 2. CR / LF / CRLF disambiguation ──────────────────────────────

#[test]
fn cr_lf_and_crlf_are_not_collapsed() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    // Three distinct writes; verify the child sees CR, LF, CRLF as
    // 4 bytes total in order. A buggy substrate that collapsed `\r`
    // followed by `\n` into one `\n` (or vice versa) would produce
    // 3 bytes; one that expanded `\n` to `\r\n` would produce 5+.
    session.write_stdin(b"\r");
    session.write_stdin(b"\n");
    session.write_stdin(b"\r\n");
    session.assert_received_exact(b"\r\n\r\n", RECEIVE_TIMEOUT);
}

// ── 3. Multi-line submission semantic ─────────────────────────────

#[test]
fn hello_newline_world_arrives_as_eleven_bytes() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"hello\nworld");
    session.assert_received_exact(b"hello\nworld", RECEIVE_TIMEOUT);
}

// ── 4. Rapid alternation preserves order ──────────────────────────

#[test]
fn rapid_alternation_of_x_and_newline_preserves_order() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    for _ in 0..100 {
        session.write_stdin(b"x");
        session.write_stdin(b"\n");
    }
    let mut expected = Vec::with_capacity(200);
    for _ in 0..100 {
        expected.extend_from_slice(b"x\n");
    }
    session.assert_received_exact(&expected, RECEIVE_TIMEOUT * 2);
}

// ── 5. One byte per write — no host-side accumulator ──────────────

#[test]
fn ten_separate_single_byte_writes_arrive_in_order() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    for i in 0u8..10 {
        // Use printable bytes so a logging hiccup is human-readable.
        session.write_stdin(std::slice::from_ref(&(b'0' + i)));
        // Brief spacing to make sure any host-side coalescer would
        // have a chance to fire; we still expect order preservation.
        std::thread::sleep(Duration::from_millis(5));
    }
    session.assert_received_exact(b"0123456789", RECEIVE_TIMEOUT);
}

// ── 6. Stdin write while child produces stdout (interleaved) ──────

#[test]
fn stdin_write_interleaves_with_child_tick_output() {
    // Child emits "T\n" every 50ms; we let it tick for ~250ms, then
    // write a single-byte marker, then keep draining to give later
    // ticks a window to arrive. The substrate guarantee under test
    // is that the host-side input write does not stall the child's
    // output stream — i.e. ticks appear *and* the marker appears.
    //
    // We deliberately assert only `tick_count >= 1` because CI
    // process scheduling makes the absolute tick count noisy
    // (observed: macOS-15 sometimes ships only 1 tick by the time
    // we see the marker echo). A single tick is sufficient to prove
    // the interleaving — see #448 commit history for the looser
    // bound rationale.
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&["--tick-ms", "50"]);
    std::thread::sleep(Duration::from_millis(250));
    session.write_stdin(b"M");
    let drained = session.drain_until_contains(b"M", RECEIVE_TIMEOUT);
    assert!(drained.contains(&b'M'), "marker M not seen in {drained:?}");
    let tick_count = drained.windows(2).filter(|w| w == b"T\n").count();
    assert!(
        tick_count >= 1,
        "expected at least 1 tick alongside marker, saw {tick_count}: {drained:?}"
    );
}

// ── 7. Bracketed-paste markers transit atomically ─────────────────

#[test]
fn bracketed_paste_markers_transit_as_sixteen_bytes() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    let payload = b"\x1b[200~hello\x1b[201~";
    session.write_stdin(payload);
    session.assert_received_exact(payload, RECEIVE_TIMEOUT);
}

// ── 8. Arbitrary escape sequence (DSR query) transits verbatim ────

#[test]
fn dsr_cursor_position_query_transits_verbatim_from_host() {
    // ConPTY's PSEUDOCONSOLE_PASSTHROUGH_MODE famously *does* respond
    // to the child's DSR query on the host side (#150 motivation).
    // We're testing the symmetric direction: when the *host* writes
    // `\x1b[6n` into the input pipe, the substrate must NOT respond;
    // it should pass the bytes through to the child verbatim.
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    session.write_stdin(b"\x1b[6n");
    session.assert_received_exact(b"\x1b[6n", RECEIVE_TIMEOUT);
}

// ── 9. No host-side echo when child suppresses it ─────────────────

#[test]
fn host_does_not_receive_input_echo_when_child_suppresses() {
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&["--no-echo"]);
    // Send 16 bytes; if ConPTY were inserting its own echo, those
    // bytes would come back. Verify the host's read pipe stays
    // empty for a full window.
    session.write_stdin(b"shibboleth\r\n!@#$");
    let observed = session.drain_for(Duration::from_millis(400), None);
    assert!(
        observed.is_empty(),
        "expected no echo from --no-echo testbin, got {} bytes: {observed:?}",
        observed.len(),
    );
}

// ── 10. Three-way parity: Win11 native / Win10 sidecar / POSIX ────

#[test]
fn three_way_byte_parity_on_hello_newline_world() {
    // The MITM substrate must be byte-exact regardless of which
    // ConPTY backend ran. On POSIX this is the PTY; on Windows we
    // route through either the native kernel32 ConPTY (Win11+) or
    // the bundled sidecar `conpty.dll` (Win10 with cached
    // redistributable, #443/#446). On Win10 *without* a sidecar the
    // test still runs — kernel32 fallback still has to byte-exact
    // the input pipe; the only thing the sidecar fixes is the
    // *output* path's passthrough-mode honoring. So this asserts the
    // input-side guarantee independent of which backend is loaded.
    //
    // If a future ConPTY-side change quietly reintroduces input-path
    // CR/LF rewriting, this test fails on the affected platform with
    // a clear byte-diff.
    skip_if_unsupported!();
    let session = EchoerSession::spawn(&[]);
    let payload = b"hello\nworld";
    let start = Instant::now();
    session.write_stdin(payload);
    session.assert_received_exact(payload, RECEIVE_TIMEOUT);
    // Sanity: the round-trip happens in well under our timeout.
    assert!(
        start.elapsed() < RECEIVE_TIMEOUT,
        "round-trip should be fast; took {:?}",
        start.elapsed()
    );
}
