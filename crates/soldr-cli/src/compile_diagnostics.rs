//! Diagnostics for a dispatched compile that failed unhelpfully.
//!
//! Extracted from `compile_dispatch.rs` for soldr#1969: that file is already
//! well past the 1,500-line ceiling, and the ratchet added in soldr#1966
//! correctly refused to let it grow further. This is the module the ratchet
//! asked for -- a new file rather than a rename of a hot one, so no in-flight
//! branch gets a modify/delete conflict.

use std::io::Write;

/// Wraps the caller's stderr sink and records whether the dispatched
/// compile wrote anything to it.
///
/// A rustc failure never surfaces as a [`DispatchError`] — it arrives as
/// `CompileDone { exit_code != 0 }` and is propagated as a bare exit code
/// (see [`client_error_indicates_daemon_unavailable`]). When the daemon
/// also relays no stderr, Cargo prints `error: could not compile <crate>`
/// with an empty cause and the user has nothing to act on.
///
/// That is the shape of soldr#1857, where ~2.8% of dispatched compiles on
/// Windows fail this way while the same workload through
/// `soldr --no-cache` is clean. Detecting "failed **and** said nothing" is
/// what separates that fault from an ordinary compile error, so it is
/// worth the one counter.
pub(crate) struct SilenceDetectingWriter<W> {
    inner: W,
    bytes_written: u64,
    /// soldr#1969: did the failure output name a path inside soldr's own
    /// cache?
    saw_cache_path: bool,
    /// Trailing bytes of the previous chunk, so a marker split across a
    /// stream boundary is still found. Streamed stderr arrives in arbitrary
    /// slices, so scanning each chunk in isolation would miss the exact case
    /// this exists to catch.
    tail: Vec<u8>,
}

/// The path segment that identifies soldr's own compile-cache storage.
///
/// Nothing in a user's build legitimately references this, so seeing it in a
/// *failure* message is strong evidence the cache is implicated rather than
/// the code being compiled.
const CACHE_OWNED_PATH_MARKER: &[u8] = b"daemon-state";

impl<W: Write> SilenceDetectingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            bytes_written: 0,
            saw_cache_path: false,
            tail: Vec::new(),
        }
    }

    /// Record whether `chunk` (joined to the previous chunk's tail) names a
    /// cache-owned path.
    fn scan_for_cache_path(&mut self, chunk: &[u8]) {
        if self.saw_cache_path {
            return;
        }
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(chunk);
        self.saw_cache_path = window
            .windows(CACHE_OWNED_PATH_MARKER.len())
            .any(|w| w == CACHE_OWNED_PATH_MARKER);
        // Keep just enough to bridge the next boundary.
        let keep = CACHE_OWNED_PATH_MARKER.len().saturating_sub(1);
        if window.len() > keep {
            window.drain(..window.len() - keep);
        }
        self.tail = window;
    }

    /// Emit a diagnostic when the compile failed without explaining why.
    ///
    /// Deliberately hedged: a non-zero exit with no output is *usually*
    /// the #1857 fault, but claiming it always is would be wrong, so the
    /// message states the observation first and the likely cause second.
    pub(crate) fn report_if_silently_failed(&mut self, exit_code: i32) {
        if exit_code == 0 {
            return;
        }
        // soldr#1969: the failure *did* explain itself, but the file it names
        // belongs to soldr. A linker error citing a path nobody recognises
        // reads as a broken toolchain or broken code -- attributing it cost a
        // full diagnostic pass in the report. Say whose file it is.
        // soldr#1999 rule 1: a warning emitted earlier in this build belongs
        // *at* the failure, not hundreds of Cargo progress lines above it.
        // Printed a second time rather than only here, because someone
        // watching live must not have to wait for a failure to learn
        // something went wrong.
        if let Some(block) = soldr_core::warning_log::replay_block() {
            let _ = write!(self.inner, "{block}");
        }
        if self.bytes_written > 0 {
            if self.saw_cache_path {
                let _ = writeln!(
                    self.inner,
                    concat!(
                        "soldr: that error names a file inside soldr's own compile cache, ",
                        "not your project.
",
                        "soldr: an intermediate can be reclaimed while a slow link is still ",
                        "reading it -- see soldr#1969.
",
                        "soldr: retrying usually succeeds; `soldr --no-cache cargo ...` ",
                        "bypasses the cache entirely."
                    )
                );
                let _ = self.inner.flush();
            }
            return;
        }
        let _ = writeln!(
            self.inner,
            "soldr: rustc exited {exit_code} without emitting any diagnostics.\n\
             soldr: the compile was dispatched to soldr-daemon and failed before it \
             could report a reason — see soldr#1857.\n\
             soldr: retrying usually succeeds; `soldr --no-cache cargo ...` bypasses \
             the daemon entirely."
        );
        let _ = self.inner.flush();
    }
}

impl<W: Write> Write for SilenceDetectingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes_written += written as u64;
        self.scan_for_cache_path(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    // soldr#1857 — a dispatched compile that fails *and* says nothing is
    // the fault signature worth calling out. These three cases pin the
    // boundary: only failure-with-silence gets the extra diagnostic.
    timed_test!(silent_failure_gets_an_explanatory_diagnostic, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("without emitting any diagnostics"),
            "expected the silence to be named; got:
{text}"
        );
        assert!(
            text.contains("soldr#1857"),
            "expected a pointer to the tracking issue; got:
{text}"
        );
        assert!(
            text.contains("--no-cache"),
            "expected the documented bypass; got:
{text}"
        );
    });

    // soldr#1999 rule 1: a warning emitted earlier in the build must appear
    // *at* the failure. Left upstream it sits hundreds of Cargo progress lines
    // above the error and nobody connects the two -- #1992's rust-lld retry
    // notice is the worked example.
    timed_test!(an_earlier_warning_is_repeated_at_the_failure, {
        soldr_core::warning_log::clear();
        soldr_core::warning_log::record("soldr warning: fast linker was unavailable");
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"error[E0308]: mismatched types");
            writer.report_if_silently_failed(1);
        }
        soldr_core::warning_log::clear();
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("fast linker was unavailable"),
            "the earlier warning must be repeated at the failure, got: {text}"
        );
    });

    // A successful build must not be decorated with a warning replay -- the
    // warning was already printed once, when it happened.
    timed_test!(a_successful_build_does_not_replay_warnings, {
        soldr_core::warning_log::clear();
        soldr_core::warning_log::record("soldr warning: something minor");
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"   Compiling foo v0.1.0");
            writer.report_if_silently_failed(0);
        }
        soldr_core::warning_log::clear();
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("something minor"),
            "exit 0 must stay quiet, got: {text}"
        );
    });

    // soldr#1969 — a linker error naming a file inside soldr's cache reads as
    // a broken toolchain. These pin that soldr says whose file it is.
    timed_test!(a_failure_naming_a_cache_path_is_attributed_to_soldr, {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(
                br"LNK1181: cannot open input file 'C:\c\zccache\daemon-state1\staging.natvis'",
            );
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("soldr's own compile cache"),
            "must attribute the file to soldr, got: {text}"
        );
        assert!(text.contains("1969"), "must cite the issue, got: {text}");
    });

    // The marker arrives split across two writes -- the exact case the rolling
    // window exists for, and the one a per-chunk scan would miss.
    timed_test!(a_cache_path_split_across_chunks_is_still_found, {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(br"LNK1181: cannot open 'C:\c\daemon");
            let _ = writer.write_all(br"-state1\staging.natvis'");
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            text.contains("soldr's own compile cache"),
            "a marker split across writes must still be detected, got: {text}"
        );
    });

    // An ordinary compile error must not be blamed on the cache.
    timed_test!(an_ordinary_failure_is_not_attributed_to_the_cache, {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"error[E0308]: mismatched types");
            writer.report_if_silently_failed(1);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("soldr's own compile cache"),
            "a normal compile error must not be blamed on soldr, got: {text}"
        );
    });

    // Success is never annotated, even if the output mentions the cache.
    timed_test!(a_successful_compile_is_never_attributed, {
        let mut sink: Vec<u8> = Vec::new();
        {
            let mut writer = SilenceDetectingWriter::new(&mut sink);
            let _ = writer.write_all(b"note: daemon-state path mentioned");
            writer.report_if_silently_failed(0);
        }
        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("1969"),
            "exit 0 must stay silent, got: {text}"
        );
    });

    timed_test!(failure_that_produced_output_is_left_alone, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // Whatever rustc already said about the failure.
        writer
            .write_all(
                b"error[E0308]: mismatched types
",
            )
            .expect("write");
        writer.report_if_silently_failed(1);

        let text = String::from_utf8(sink).expect("utf8");
        assert!(
            !text.contains("soldr#1857"),
            "an ordinary compile error must not be blamed on #1857; got:
{text}"
        );
        assert_eq!(
            text,
            "error[E0308]: mismatched types
"
        );
    });

    timed_test!(successful_silent_compile_is_left_alone, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        // The overwhelmingly common case: a cache hit says nothing and succeeds.
        writer.report_if_silently_failed(0);

        assert!(sink.is_empty(), "exit 0 must stay silent");
    });

    timed_test!(byte_count_tracks_partial_writes, {
        let mut sink: Vec<u8> = Vec::new();
        let mut writer = SilenceDetectingWriter::new(&mut sink);
        writer.write_all(b"abc").expect("write");
        writer.write_all(b"de").expect("write");

        assert_eq!(writer.bytes_written, 5);
    });
}
