//! Consumer conformance test kit for the v1 broker integration (#415).
//!
//! Concrete coverage every consumer daemon (zccache, soldr, fbuild,
//! clud) needs once it adopts the v1 backend SDK:
//!
//! 1. **Golden-bytes assertions** — freeze the on-wire encoding of
//!    consumer payload frames so a future prost upgrade or accidental
//!    field renumber breaks the build instead of the deployed fleet.
//! 2. **Live probe assertion** — point [`probe_responds_correctly`] at a
//!    running endpoint; it runs the real
//!    [`BackendHandle::probe_with_service`] handshake and validates the
//!    response identity matches.
//! 3. **Mixed-wire harness** — drive a sequence of legacy bytes, opaque
//!    `Frame` traffic, and `BackendHandle` probes through one
//!    [`BackendEndpointMux`] without any sockets and assert each chunk
//!    is classified the way the consumer expects, including the
//!    intentionally ambiguous leading-`0x01` legacy case.
//!
//! The acceptance bar from the post-mortem is that a consumer gets the
//! coverage from this kit in roughly 30 lines of test code; the in-repo
//! self-conformance test
//! `crates/running-process/tests/broker/conformance_kit.rs` proves that
//! number using ONLY the surface exported below.
//!
//! [`BackendHandle::probe_with_service`]: crate::broker::backend_handle::BackendHandle::probe_with_service
//! [`BackendEndpointMux`]: crate::broker::backend_sdk::BackendEndpointMux

use std::fmt::Write as _;

use crate::broker::backend_handle::{BackendHandle, DaemonProcess};
use crate::broker::backend_sdk::{BackendEndpointMux, LegacyClassification, MuxError, MuxPoll};
use crate::broker::protocol::{encode_framed, try_decode_framed, Endpoint, Frame};

/// Error from a [`probe_responds_correctly`] call.
///
/// All variants mean the endpoint is NOT serving a SDK-compatible probe
/// reply; the consumer's accept-loop wiring needs to be fixed before
/// shipping.
#[derive(Debug, thiserror::Error)]
pub enum ConformanceError {
    /// The live probe failed (peer dead, framing violation, identity
    /// mismatch, etc).
    #[error("BackendHandle probe failed: {0}")]
    Probe(String),
    /// The probe returned, but identity fields disagree with `expected`.
    #[error("probed identity does not match expected: {0}")]
    IdentityMismatch(String),
    /// A mixed-wire step did not classify the way the test predicted.
    #[error("mux verdict mismatch at step {step}: expected {expected}, got {got}")]
    UnexpectedVerdict {
        /// Zero-based index of the step.
        step: usize,
        /// Predicted classification.
        expected: String,
        /// What the mux returned.
        got: String,
    },
    /// A mixed-wire step expected a [`MuxError`] but the mux succeeded
    /// (or returned a different error).
    #[error("mux error mismatch at step {step}: {detail}")]
    UnexpectedMuxError {
        /// Zero-based index of the step.
        step: usize,
        /// Human-readable detail.
        detail: String,
    },
    /// A frame body did not encode to the recorded golden bytes.
    #[error(
        "framed frame did not match golden bytes:\n  expected ({expected_len} bytes): {expected}\n  got      ({got_len} bytes): {got}"
    )]
    GoldenMismatch {
        /// Length of the recorded golden frame.
        expected_len: usize,
        /// Hex-encoded golden bytes.
        expected: String,
        /// Length of the freshly-encoded frame.
        got_len: usize,
        /// Hex-encoded actual bytes.
        got: String,
    },
}

// ---------------------------------------------------------------------------
// 1) Golden-bytes assertion helpers.
// ---------------------------------------------------------------------------

/// Assert that `frame` framed with the v1 outer header
/// (`[0x01][u32 LE body_len][prost Frame]`) encodes to exactly
/// `golden_bytes`.
///
/// A mismatch returns [`ConformanceError::GoldenMismatch`] with both
/// sides hex-encoded so CI logs show the diff without a debugger.
///
/// Consumers freeze the expected bytes the same way the in-repo
/// `tests/broker/golden_bytes.rs` does: encode once, paste the array,
/// then never regenerate it from the encoder under test. Use one
/// recorded sample per consumer payload protocol.
///
/// ```
/// use running_process::broker::protocol::Frame;
/// use running_process::test_support::conformance::{
///     assert_framed_frame_matches_golden, encode_framed_for_golden,
/// };
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Record once and freeze:
/// let frame = Frame::request(0xF412, b"ping".to_vec()).with_request_id(1);
/// let golden = encode_framed_for_golden(&frame)?;
/// // Then in the actual test the golden array is checked-in literal:
/// assert_framed_frame_matches_golden(&frame, &golden)?;
/// # Ok(())
/// # }
/// ```
pub fn assert_framed_frame_matches_golden(
    frame: &Frame,
    golden_bytes: &[u8],
) -> Result<(), ConformanceError> {
    let encoded = encode_framed(frame).map_err(|err| ConformanceError::GoldenMismatch {
        expected_len: golden_bytes.len(),
        expected: hex(golden_bytes),
        got_len: 0,
        got: format!("<encode error: {err}>"),
    })?;
    if encoded == golden_bytes {
        return Ok(());
    }
    Err(ConformanceError::GoldenMismatch {
        expected_len: golden_bytes.len(),
        expected: hex(golden_bytes),
        got_len: encoded.len(),
        got: hex(&encoded),
    })
}

/// Helper for recording new golden bytes: returns the framed wire bytes
/// the consumer should paste into the `const GOLDEN_…` literal.
///
/// Not called from the test path — only used once when the consumer
/// adds a new recorded sample.
pub fn encode_framed_for_golden(
    frame: &Frame,
) -> Result<Vec<u8>, crate::broker::protocol::FramingError> {
    encode_framed(frame)
}

/// Assert that `golden_bytes` decodes (framed v1) back into a `Frame`
/// whose `payload_protocol`, `kind`, `request_id`, and `payload` match
/// `expected_frame`. The trace context and encoding fields are
/// intentionally not compared so consumers can use an `expected_frame`
/// constructed with [`Frame::request`]/[`Frame::response_to`] defaults.
///
/// Together with [`assert_framed_frame_matches_golden`] this proves the
/// consumer's payload format round-trips byte-for-byte.
///
/// ```
/// use running_process::broker::protocol::Frame;
/// use running_process::test_support::conformance::{
///     assert_framed_frame_matches_golden, assert_framed_bytes_decode_to,
///     encode_framed_for_golden,
/// };
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let frame = Frame::request(0xF412, b"ping".to_vec()).with_request_id(1);
/// let golden = encode_framed_for_golden(&frame)?;
/// assert_framed_frame_matches_golden(&frame, &golden)?;
/// assert_framed_bytes_decode_to(&golden, &frame)?;
/// # Ok(())
/// # }
/// ```
pub fn assert_framed_bytes_decode_to(
    golden_bytes: &[u8],
    expected_frame: &Frame,
) -> Result<(), ConformanceError> {
    let decoded = try_decode_framed(golden_bytes)
        .map_err(|err| ConformanceError::GoldenMismatch {
            expected_len: golden_bytes.len(),
            expected: hex(golden_bytes),
            got_len: 0,
            got: format!("<decode error: {err}>"),
        })?
        .ok_or_else(|| ConformanceError::GoldenMismatch {
            expected_len: golden_bytes.len(),
            expected: hex(golden_bytes),
            got_len: 0,
            got: "<short read: golden bytes did not contain a complete frame>".to_string(),
        })?;
    if decoded.consumed != golden_bytes.len() {
        return Err(ConformanceError::GoldenMismatch {
            expected_len: golden_bytes.len(),
            expected: hex(golden_bytes),
            got_len: decoded.consumed,
            got: format!(
                "<trailing bytes: consumed {} of {}>",
                decoded.consumed,
                golden_bytes.len()
            ),
        });
    }
    let frame = decoded.frame;
    if frame.payload_protocol != expected_frame.payload_protocol
        || frame.kind != expected_frame.kind
        || frame.request_id != expected_frame.request_id
        || frame.payload != expected_frame.payload
    {
        return Err(ConformanceError::IdentityMismatch(format!(
            "decoded frame fields differ: \
             payload_protocol {:#06X} vs {:#06X}, kind {} vs {}, \
             request_id {} vs {}, payload_len {} vs {}",
            frame.payload_protocol,
            expected_frame.payload_protocol,
            frame.kind,
            expected_frame.kind,
            frame.request_id,
            expected_frame.request_id,
            frame.payload.len(),
            expected_frame.payload.len(),
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{byte:02X}");
    }
    out
}

// ---------------------------------------------------------------------------
// 2) Live BackendHandle probe assertion.
// ---------------------------------------------------------------------------

/// Run a real [`BackendHandle::probe_with_service`] handshake against
/// the consumer's running endpoint and assert the response identity
/// matches `expected`.
///
/// This is the live counterpart to the sans-io probe coverage in
/// [`MixedWireScenario`] — it proves the consumer's accept loop wires
/// the [`BackendEndpointMux`]'s probe reply onto the socket correctly.
///
/// The probe is sent with `service_name` and `service_version` so the
/// returned handle reflects the consumer's logical service tuple; the
/// returned [`BackendHandle`] is discarded after identity validation.
///
/// Blocking: this opens a TCP/Unix/pipe connection and reads the reply
/// synchronously. Consumers test it from a `#[test]` body without an
/// async runtime (matching the rest of the SDK's blocking surface).
///
/// ```no_run
/// use running_process::broker::backend_handle::DaemonProcess;
/// use running_process::broker::protocol::Endpoint;
/// use running_process::test_support::conformance::probe_responds_correctly;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let endpoint = Endpoint::unix_socket("my-daemon", "/tmp/my-daemon.sock")?;
/// let expected = DaemonProcess::current_process(endpoint.clone(), Some(30))?;
/// // Daemon already running on `endpoint` and serving via BackendEndpointMux.
/// probe_responds_correctly("my-daemon", "1.0.0", &endpoint, &expected)?;
/// # Ok(())
/// # }
/// ```
pub fn probe_responds_correctly(
    service_name: &str,
    service_version: &str,
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<(), ConformanceError> {
    let handle =
        BackendHandle::probe_with_service(service_name, service_version, endpoint, expected)
            .map_err(|err| ConformanceError::Probe(err.to_string()))?;
    if handle.daemon_process.pid != expected.pid {
        return Err(ConformanceError::IdentityMismatch(format!(
            "pid {} (probed) vs {} (expected)",
            handle.daemon_process.pid, expected.pid
        )));
    }
    if handle.daemon_process.ipc_endpoint != expected.ipc_endpoint {
        return Err(ConformanceError::IdentityMismatch(format!(
            "endpoint {:?} (probed) vs {:?} (expected)",
            handle.daemon_process.ipc_endpoint, expected.ipc_endpoint
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3) Mixed-wire sans-io harness.
// ---------------------------------------------------------------------------

/// One scripted step of bytes fed at a [`BackendEndpointMux`].
///
/// The harness appends `bytes` to its in-memory read buffer and calls
/// [`BackendEndpointMux::poll`] once, then asserts the verdict matches
/// `expect`. Use [`MixedWireExpect::ProbeAnswered`] /
/// [`MixedWireExpect::Payload`] to advance the buffer the same way a
/// real accept loop would.
#[derive(Debug, Clone)]
pub struct MixedWireStep {
    /// Bytes pushed into the buffer before polling.
    pub bytes: Vec<u8>,
    /// Predicted mux verdict.
    pub expect: MixedWireExpect,
}

/// Predicted outcome of one [`MixedWireStep`].
#[derive(Debug, Clone)]
pub enum MixedWireExpect {
    /// The poll must return [`MuxPoll::NeedMoreBytes`].
    NeedMoreBytes,
    /// The poll must return [`MuxPoll::Legacy`].
    Legacy,
    /// The poll must return [`MuxPoll::ProbeAnswered { .. }`]. The
    /// `consumed` bytes are drained from the buffer; the `reply` is
    /// returned to the caller for further assertions (not checked
    /// here).
    ProbeAnswered,
    /// The poll must return [`MuxPoll::Payload { .. }`] with the given
    /// payload protocol. The frame bytes are drained.
    Payload {
        /// Required payload protocol on the decoded frame.
        payload_protocol: u32,
    },
    /// The poll must surface a [`MuxError`] whose `Debug` string
    /// contains `error_contains`.
    Error {
        /// Substring required in the error's `Debug` form.
        error_contains: String,
    },
}

/// Driver for [`MixedWireStep`]s.
///
/// Drives a single shared read buffer through the mux, the same way a
/// real accept loop does. Steps consume buffered bytes as the
/// classification dictates, so a sequence of `Legacy` → `Frame` →
/// `Probe` → `Frame` traffic exercises the disambiguation logic in one
/// long-lived buffer.
///
/// The intentionally tricky case to script: a legacy header whose first
/// byte equals `0x01` (the v1 framing version byte). The mux defers to
/// the consumer's `legacy_detector` for that disambiguation; this
/// harness lets the consumer assert the detector wins.
///
/// ```
/// use running_process::broker::backend_handle::DaemonProcess;
/// use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification};
/// use running_process::broker::protocol::{encode_framed, Endpoint, Frame};
/// use running_process::test_support::conformance::{
///     MixedWireExpect, MixedWireScenario, MixedWireStep,
/// };
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let endpoint = Endpoint::unix_socket("demo", "/tmp/demo.sock")?;
/// let daemon = DaemonProcess::current_process(endpoint, Some(30))?;
/// let mux = BackendEndpointMux::new(daemon, &[0xF412], |buf: &[u8]| {
///     match buf.first() {
///         None => LegacyClassification::NeedMoreBytes,
///         Some(b'L') => LegacyClassification::Legacy,
///         Some(_) => LegacyClassification::NotLegacy,
///     }
/// });
/// let frame = Frame::request(0xF412, b"ping".to_vec()).with_request_id(1);
/// let frame_wire = encode_framed(&frame)?;
/// MixedWireScenario::new()
///     .step(MixedWireStep {
///         bytes: b"L\x00hello".to_vec(),
///         expect: MixedWireExpect::Legacy,
///     })
///     .step(MixedWireStep {
///         bytes: frame_wire,
///         expect: MixedWireExpect::Payload { payload_protocol: 0xF412 },
///     })
///     .run(&mux)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default, Clone)]
pub struct MixedWireScenario {
    steps: Vec<MixedWireStep>,
}

impl MixedWireScenario {
    /// Empty scenario.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Append a step (builder style).
    pub fn step(mut self, step: MixedWireStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Drive each step in order against `mux`. After a `ProbeAnswered`
    /// or `Payload` expectation, the consumed bytes are drained; after
    /// `Legacy`, the buffer is cleared (real consumers would hand it to
    /// their decoder); after `NeedMoreBytes` or `Error`, the buffer is
    /// preserved for the next step.
    pub fn run<F>(self, mux: &BackendEndpointMux<F>) -> Result<(), ConformanceError>
    where
        F: Fn(&[u8]) -> LegacyClassification,
    {
        let mut buf: Vec<u8> = Vec::new();
        for (idx, step) in self.steps.into_iter().enumerate() {
            buf.extend_from_slice(&step.bytes);
            match (&step.expect, mux.poll(&buf)) {
                (MixedWireExpect::NeedMoreBytes, Ok(MuxPoll::NeedMoreBytes)) => {}
                (MixedWireExpect::Legacy, Ok(MuxPoll::Legacy)) => {
                    buf.clear();
                }
                (MixedWireExpect::ProbeAnswered, Ok(MuxPoll::ProbeAnswered { consumed, .. })) => {
                    buf.drain(..consumed);
                }
                (
                    MixedWireExpect::Payload { payload_protocol },
                    Ok(MuxPoll::Payload { frame, consumed }),
                ) => {
                    if frame.payload_protocol != *payload_protocol {
                        return Err(ConformanceError::UnexpectedVerdict {
                            step: idx,
                            expected: format!("Payload protocol {payload_protocol:#06X}"),
                            got: format!("Payload protocol {:#06X}", frame.payload_protocol),
                        });
                    }
                    buf.drain(..consumed);
                }
                (MixedWireExpect::Error { error_contains }, Err(err)) => {
                    let rendered = format!("{err:?}");
                    if !rendered.contains(error_contains) {
                        return Err(ConformanceError::UnexpectedMuxError {
                            step: idx,
                            detail: format!(
                                "expected substring {error_contains:?} in {rendered:?}"
                            ),
                        });
                    }
                    // Connection-fatal: clear remaining bytes.
                    buf.clear();
                }
                (expect, Ok(verdict)) => {
                    return Err(ConformanceError::UnexpectedVerdict {
                        step: idx,
                        expected: describe_expect(expect),
                        got: describe_verdict(&verdict),
                    });
                }
                (_, Err(err)) => {
                    return Err(ConformanceError::UnexpectedMuxError {
                        step: idx,
                        detail: format!("mux returned unexpected error: {err:?}"),
                    });
                }
            }
        }
        Ok(())
    }
}

fn describe_expect(expect: &MixedWireExpect) -> String {
    match expect {
        MixedWireExpect::NeedMoreBytes => "NeedMoreBytes".to_string(),
        MixedWireExpect::Legacy => "Legacy".to_string(),
        MixedWireExpect::ProbeAnswered => "ProbeAnswered".to_string(),
        MixedWireExpect::Payload { payload_protocol } => {
            format!("Payload(protocol={payload_protocol:#06X})")
        }
        MixedWireExpect::Error { error_contains } => {
            format!("Error(contains={error_contains:?})")
        }
    }
}

fn describe_verdict(verdict: &MuxPoll) -> String {
    match verdict {
        MuxPoll::NeedMoreBytes => "NeedMoreBytes".to_string(),
        MuxPoll::Legacy => "Legacy".to_string(),
        MuxPoll::ProbeAnswered { consumed, .. } => format!("ProbeAnswered(consumed={consumed})"),
        MuxPoll::Payload { frame, consumed } => format!(
            "Payload(protocol={:#06X}, consumed={consumed})",
            frame.payload_protocol
        ),
    }
}

// Force the unused MuxError import to be referenced in non-test builds
// (it appears only in error-path doc references above).
#[allow(dead_code)]
type _MuxErrorAlias = MuxError;
