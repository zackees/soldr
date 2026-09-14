//! Output sink for streamed compile results (soldr#2388 Step 5 / soldr#2365).
//!
//! The daemon runs the embedded-zccache compile **once** and streams the
//! captured stdout/stderr/exit through a [`CompileOutputSink`]. Its
//! implementation is the SESSION `0x5350` sink
//! (`session_sink::SessionCompileSink`), which encodes
//! `SessionFrame::Stdout/Stderr/Exit` via running-process `session_codec` and
//! carries `cache_outcome` / `compile_id` on `SessionExit.metadata`. The legacy
//! direct-IPC sink was deleted with that wire in soldr#2424.
//!
//! Execution model (fable5 ruling on #2365, answer A): soldr owns execution via
//! the embedded zccache service; this sink is transport-only. Per-compile
//! telemetry (`compile_trace`, tracing) stays in the caller, not the sink.

/// Transport for one compile's streamed output. The daemon's compile engine is
/// generic over this so the embedded-zccache execution is shared across wires.
///
/// `async fn` in a crate-internal trait: the `async_fn_in_trait` lint warns
/// about the absent `Send` bound for *public* traits; this trait never leaves
/// the crate and its only callers `.await` the futures inline on the daemon's
/// own task, so the auto-trait leakage the lint guards against cannot occur.
#[allow(async_fn_in_trait)]
pub(crate) trait CompileOutputSink {
    /// Emit one stdout chunk (already sized to at most `CHUNK_BYTES`).
    async fn emit_stdout_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    /// Emit one stderr chunk.
    async fn emit_stderr_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    /// Emit the terminal result: exit code, cache attribution, and the
    /// per-compile id, which the SESSION sink carries on `SessionExit.metadata`.
    async fn emit_done(
        &mut self,
        exit_code: i32,
        cached: bool,
        cache_outcome: i32,
        compile_id: &str,
    ) -> std::io::Result<()>;
}
