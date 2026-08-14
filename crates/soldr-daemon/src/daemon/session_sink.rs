//! SESSION `0x5350` output sink (soldr#2388 Step 6 / #2365).
//!
//! The [`CompileOutputSink`](crate::daemon::compile_sink::CompileOutputSink)
//! implementation for the SESSION wire: it renders the embedded-zccache
//! compile's captured stdout/stderr/exit as running-process `SessionFrame`s,
//! encoded with `session_codec` onto the `[1][u32 len][Frame{0x5350}]` wire the
//! broker relays transparently. The terminal `Exit` frame carries `cache_outcome`
//! and `compile_id` on `SessionExit.metadata` (running-process#934), so the
//! SESSION path preserves the observability the legacy `CompileDone` frame gives
//! (build history, `soldr doctor`, perf-gate attribution).
//!
//! Execution is unchanged — soldr owns it via the embedded zccache service; this
//! sink only re-frames the *output* (fable5's answer-A ruling on #2365).

use std::collections::HashMap;

use running_process::broker::protocol_v2::{session_frame, SessionExit, SessionFrame};
use running_process::broker::session_codec::encode_session_frame;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::daemon::compile_sink::CompileOutputSink;

/// Metadata key: the daemon's cache outcome (an `i32` `CacheOutcome` discriminant,
/// stringified). Consumed by the wrapper/build-history on the SESSION path.
pub(crate) const META_CACHE_OUTCOME: &str = "cache_outcome";
/// Metadata key: the per-compile id.
pub(crate) const META_COMPILE_ID: &str = "compile_id";

/// A [`CompileOutputSink`] that writes SESSION-lane `SessionFrame`s to `writer`.
pub(crate) struct SessionCompileSink<'a, W> {
    writer: &'a mut W,
    /// Session-local outbound sequence, carried in each frame's envelope
    /// `request_id` for observability (see `session_codec`).
    seq: u64,
}

impl<'a, W> SessionCompileSink<'a, W> {
    pub(crate) fn new(writer: &'a mut W) -> Self {
        Self { writer, seq: 0 }
    }
}

impl<W: AsyncWrite + Unpin> SessionCompileSink<'_, W> {
    async fn write_frame(&mut self, frame: SessionFrame) -> std::io::Result<()> {
        let bytes = encode_session_frame(&frame, self.seq)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        self.seq = self.seq.wrapping_add(1);
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await
    }
}

impl<W: AsyncWrite + Unpin> CompileOutputSink for SessionCompileSink<'_, W> {
    async fn emit_stdout_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.write_frame(SessionFrame {
            kind: Some(session_frame::Kind::Stdout(chunk.to_vec())),
        })
        .await
    }

    async fn emit_stderr_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.write_frame(SessionFrame {
            kind: Some(session_frame::Kind::Stderr(chunk.to_vec())),
        })
        .await
    }

    async fn emit_done(
        &mut self,
        exit_code: i32,
        _cached: bool,
        cache_outcome: i32,
        compile_id: &str,
    ) -> std::io::Result<()> {
        // `cached` is redundant with `cache_outcome` on this wire; carry the
        // richer discriminant plus the compile id on the terminal frame.
        let mut metadata = HashMap::new();
        metadata.insert(META_CACHE_OUTCOME.to_string(), cache_outcome.to_string());
        metadata.insert(META_COMPILE_ID.to_string(), compile_id.to_string());
        self.write_frame(SessionFrame {
            kind: Some(session_frame::Kind::Exit(SessionExit {
                code: exit_code,
                signal: 0,
                metadata,
            })),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::compile_sink::CompileOutputSink as _;
    use running_process::broker::session_codec::try_decode_session_frame;

    #[test]
    fn session_sink_frames_round_trip_through_session_codec() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut buf: Vec<u8> = Vec::new();
                {
                    let mut sink = SessionCompileSink::new(&mut buf);
                    sink.emit_stdout_chunk(b"HELLO").await.unwrap();
                    sink.emit_stderr_chunk(b"WARN").await.unwrap();
                    sink.emit_done(7, true, 2, "c000000a").await.unwrap();
                }

                // Decode the three frames back off the wire and check fidelity.
                let mut rest = &buf[..];
                let mut kinds = Vec::new();
                while let Some(decoded) = try_decode_session_frame(rest).unwrap() {
                    kinds.push(decoded.frame.kind.clone().unwrap());
                    rest = &rest[decoded.consumed..];
                }
                assert!(rest.is_empty(), "all bytes consumed");
                assert_eq!(kinds.len(), 3, "stdout + stderr + exit");
                assert!(matches!(&kinds[0], session_frame::Kind::Stdout(b) if b == b"HELLO"));
                assert!(matches!(&kinds[1], session_frame::Kind::Stderr(b) if b == b"WARN"));
                match &kinds[2] {
                    session_frame::Kind::Exit(e) => {
                        assert_eq!(e.code, 7);
                        assert_eq!(
                            e.metadata.get(META_CACHE_OUTCOME).map(String::as_str),
                            Some("2")
                        );
                        assert_eq!(
                            e.metadata.get(META_COMPILE_ID).map(String::as_str),
                            Some("c000000a")
                        );
                    }
                    other => panic!("expected Exit, got {other:?}"),
                }
            });
    }
}
