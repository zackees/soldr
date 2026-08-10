//! SESSION `0x5350` compile serve — the codec-bridge (soldr#2388 Step 6c / #2365).
//!
//! fable5's answer-A ruling: a SESSION compile is a **transport swap**, not a new
//! execution path. This handler reads the opening `SessionStart` (raw rustc argv +
//! env + cwd carried on the wire), converts it with the *shared* daemon-side
//! parser ([`build_compile_request_from`]), runs it through the **embedded zccache
//! service** — the exact same execution `Request::Compile` uses — and streams the
//! captured stdout/stderr/exit back as `SessionFrame`s via the shared
//! [`stream_compile_output`](crate::daemon::server::stream_compile_output) with a
//! [`SessionCompileSink`]. There is **no spawned child at the session layer**;
//! zccache owns any rustc child internally.
//!
//! Failure attribution (no-silent-fallback invariant): a compile-service error is
//! rendered as a `SessionFrame::Stderr` diagnostic plus a terminal `Exit` carrying
//! the distinct [`SESSION_INFRA_EXIT_CODE`] — never a silent close, and never a
//! bare compiler verdict.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use running_process::broker::protocol_v2::{session_frame, SessionStart};
use running_process::broker::session_codec::try_decode_session_frame;

use crate::core::SoldrPaths;
use crate::daemon::compile_request::build_compile_request_from;
use crate::daemon::compile_sink::CompileOutputSink;
use crate::daemon::server::{next_compile_id, stream_compile_output};
use crate::daemon::session_sink::SessionCompileSink;
use crate::zccache_embedded::SoldrZccacheService;

/// Distinct nonzero exit surfaced when the SESSION compile fails for an
/// infrastructure reason (compile-service error / protocol error) rather than a
/// compiler verdict. Chosen outside rustc's own exit range so the wrapper can
/// attribute it to soldr, not to the code being compiled.
pub(crate) const SESSION_INFRA_EXIT_CODE: i32 = 111;
/// `cache_outcome` sentinel for an infra failure (not a real cache decision).
const SESSION_INFRA_CACHE_OUTCOME: i32 = -1;

/// Serve one SESSION compile connection: read the opening `SessionStart`, build
/// the request via the shared parser, and dispatch it through the embedded
/// zccache service, streaming output back as `SessionFrame`s.
///
/// # Errors
///
/// A transport error, or a protocol error reading the opening frame. A
/// compile-service error is NOT an `Err` — it is reported to the client as a
/// diagnostic `Stderr` + infra `Exit` (never a silent close).
pub(crate) async fn serve_session_compile<IO>(
    mut io: IO,
    compile_service: &SoldrZccacheService,
    paths: &SoldrPaths,
) -> std::io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let start = read_session_start(&mut io).await?;

    // SessionStart carries the command the client would have exec'd:
    // `program` is the compiler path, `args` the compiler arguments. The shared
    // parser expects `[compiler, ...args]` (argv[0] = compiler).
    let mut argv = Vec::with_capacity(start.args.len() + 1);
    argv.push(start.program);
    argv.extend(start.args);
    let env: Vec<(String, String)> = start
        .env
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect();
    let req = build_compile_request_from(&argv, start.cwd, env);

    dispatch_compile_session(compile_service, paths, req, &mut io).await
}

/// Run `req` through the embedded zccache service and stream its output as
/// `SessionFrame`s. Reuses the same execution + output plumbing as the legacy
/// wire (`SoldrZccacheService::compile` + `stream_compile_output`).
async fn dispatch_compile_session<IO>(
    compile_service: &SoldrZccacheService,
    paths: &SoldrPaths,
    req: crate::daemon::protocol::CompileRequest,
    io: &mut IO,
) -> std::io::Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    let compile_id = next_compile_id();
    let total = std::time::Instant::now();
    let lifecycle = req.lifecycle.clone();
    let inner_started = std::time::Instant::now();

    let body = match compile_service.compile(req).await {
        Ok(body) => body,
        Err(err) => {
            // Infra failure — report it to the client, never a silent close and
            // never a compiler verdict (fable5 no-silent-fallback / infra≠verdict).
            let mut sink = SessionCompileSink::new(io);
            let msg = format!(
                "soldr: SESSION compile could not run (infrastructure error, not a \
                 compiler error): {err}\n"
            );
            sink.emit_stderr_chunk(msg.as_bytes()).await?;
            sink.emit_done(
                SESSION_INFRA_EXIT_CODE,
                false,
                SESSION_INFRA_CACHE_OUTCOME,
                &compile_id,
            )
            .await?;
            return Ok(());
        }
    };

    let mut sink = SessionCompileSink::new(io);
    stream_compile_output(
        &mut sink,
        &body,
        paths,
        &compile_id,
        lifecycle.as_ref(),
        inner_started,
        total,
    )
    .await
}

/// Read the mandatory opening `SessionStart` frame off `io` using the sans-io
/// `session_codec` (`try_decode_session_frame`) over a growing read buffer —
/// exactly `[1][u32 len][Frame{0x5350}]` bytes, no over-read past the frame.
async fn read_session_start<IO>(io: &mut IO) -> std::io::Result<SessionStart>
where
    IO: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match try_decode_session_frame(&buf) {
            Ok(Some(decoded)) => {
                return match decoded.frame.kind {
                    Some(session_frame::Kind::Start(start)) => Ok(start),
                    other => Err(std::io::Error::other(format!(
                        "SESSION must open with SessionStart, got {other:?}"
                    ))),
                };
            }
            Ok(None) => {
                let n = io.read(&mut chunk).await?;
                if n == 0 {
                    return Err(std::io::Error::other(
                        "SESSION connection closed before SessionStart",
                    ));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(err) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err));
            }
        }
    }
}

#[cfg(test)]
mod tests;
