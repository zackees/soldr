//! Output sink for streamed compile results (soldr#2388 Step 5 / soldr#2365).
//!
//! `dispatch_compile_streaming` runs the embedded-zccache compile **once** and
//! streams the captured stdout/stderr/exit through a [`CompileOutputSink`], so
//! the one execution path serves both wires:
//!
//! - the legacy DaemonRequest wire — [`LegacyDaemonSink`] writes
//!   `Response::CompileStdoutChunk` / `CompileStderrChunk` / `CompileDone`,
//!   **byte-identical** to the pre-refactor wire (locked by
//!   `tests/phase5_contract.rs`);
//! - the SESSION `0x5350` wire — a `SessionFrame` sink (added with soldr's
//!   SESSION endpoint, #2365 Step 6) encodes `SessionFrame::Stdout/Stderr/Exit`
//!   via running-process `session_codec`, carrying `cache_outcome` / `compile_id`
//!   on `SessionExit.metadata`.
//!
//! Execution model (fable5 ruling on #2365, answer A): soldr owns execution via
//! the embedded zccache service; this sink is transport-only. Per-compile
//! telemetry (`compile_trace`, tracing) stays in the caller, not the sink.

use tokio::io::AsyncWrite;

use crate::daemon::ipc::write_frame_async;
use crate::daemon::protocol::Response;

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
    /// per-compile id. Legacy drops the id (it was always empty on the wire);
    /// the SESSION sink carries it on `SessionExit.metadata`.
    async fn emit_done(
        &mut self,
        exit_code: i32,
        cached: bool,
        cache_outcome: i32,
        compile_id: &str,
    ) -> std::io::Result<()>;
}

/// Legacy DaemonRequest sink — byte-identical to the pre-refactor wire
/// (`tests/phase5_contract.rs`). `compile_id` is intentionally NOT put on the
/// wire: `Response::CompileDone.compile_id` has always been empty here.
pub(crate) struct LegacyDaemonSink<'a, S> {
    /// The open IPC connection to the wrapper.
    pub stream: &'a mut S,
}

impl<S> CompileOutputSink for LegacyDaemonSink<'_, S>
where
    S: AsyncWrite + Unpin,
{
    async fn emit_stdout_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        write_frame_async(self.stream, &Response::CompileStdoutChunk(chunk.to_vec())).await
    }

    async fn emit_stderr_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        write_frame_async(self.stream, &Response::CompileStderrChunk(chunk.to_vec())).await
    }

    async fn emit_done(
        &mut self,
        exit_code: i32,
        cached: bool,
        cache_outcome: i32,
        _compile_id: &str,
    ) -> std::io::Result<()> {
        write_frame_async(
            self.stream,
            &Response::CompileDone {
                exit_code,
                cached,
                cache_outcome,
                compile_id: String::new(),
            },
        )
        .await
    }
}

// Keep `AsyncWriteExt` in scope: `write_frame_async` flushes through it, and a
// future in-crate sink may write directly. Referencing it here documents the
// dependency without an unused-import warning.
#[allow(unused_imports)]
use AsyncWriteExt as _KeepAsyncWriteExt;
