//! Drop cargo's cold-tree fingerprint "error" records from the terminal copy
//! of its stderr.
//!
//! The host lane sets `CARGO_LOG=cargo::core::compiler::fingerprint=info` so
//! the soldr#3040 third-party ratchet can read cargo's `fingerprint dirty for
//! <crate>` lines -- the only place cargo says *why* it recompiled a unit.
//! That level also unlocks the other INFO record in the same module: for a
//! unit whose saved fingerprint cannot be read, `log_compare` prints
//!
//! ```text
//! INFO prepare_target{..}: cargo::core::compiler::fingerprint: fingerprint error for <unit>
//! INFO prepare_target{..}: cargo::core::compiler::fingerprint:     err: failed to read `.../.fingerprint/<unit>`
//!
//! Caused by:
//!     No such file or directory (os error 2)
//! ```
//!
//! That is cargo's normal control flow on a fresh tree -- `Err` is how its
//! freshness check spells "rebuild" -- and it fires once per unit, so a cold
//! host-validation run emits a few thousand of them before compiling
//! anything. Nothing reads them: `analyze_compile_journal.py` matches only
//! `fingerprint dirty for`. This filter removes the record from what the
//! human sees. It wraps the terminal writer only; the raw byte channel that
//! feeds diagnostics and the build log is untouched.

use std::io::Write;

const ERROR_RECORD_START: &[u8] = b"cargo::core::compiler::fingerprint: fingerprint error for ";
const ERROR_RECORD_ERR: &[u8] = b"cargo::core::compiler::fingerprint:     err:";

/// Line-buffered writer that swallows cold-miss fingerprint records.
pub(crate) struct FingerprintNoiseFilter<W: Write> {
    inner: W,
    pending: Vec<u8>,
    in_record: bool,
}

impl<W: Write> FingerprintNoiseFilter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            in_record: false,
        }
    }

    fn emit_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if contains(line, ERROR_RECORD_START) || contains(line, ERROR_RECORD_ERR) {
            self.in_record = true;
            return Ok(());
        }
        if self.in_record && is_error_continuation(line) {
            return Ok(());
        }
        self.in_record = false;
        self.inner.write_all(line)
    }
}

/// The lines anyhow's `{:?}` prints after the `err:` line: a blank, a
/// `Caused by:` header, indented cause text, and (when backtraces are on) a
/// `Stack backtrace:` header with indented frames.
fn is_error_continuation(line: &[u8]) -> bool {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    if is_tracing_line(body) {
        // cargo's tracing lines are themselves indented (right-aligned
        // uptime), so a new INFO record must end the previous one before
        // the indentation rule below gets a look.
        return false;
    }
    body.is_empty()
        || body.starts_with(b"Caused by:")
        || body.starts_with(b"Stack backtrace:")
        || body.starts_with(b"  ")
}

fn is_tracing_line(body: &[u8]) -> bool {
    [
        b" INFO " as &[u8],
        b" WARN ",
        b" DEBUG ",
        b" TRACE ",
        b" ERROR ",
    ]
    .iter()
    .any(|level| contains(body, level))
}

const DIRTY_RECORD_START: &str = "cargo::core::compiler::fingerprint: fingerprint dirty for ";
const DIRTY_RECORD_REASON: &str = "cargo::core::compiler::fingerprint:     dirty: ";

/// Pull every `fingerprint dirty for <name> v<version>/...` record, with the
/// `dirty: <reason>` line under it, out of cargo's captured stderr. Same
/// grammar as `.github/scripts/analyze_compile_journal.py`'s `DIRTY_RE`.
pub(crate) fn extract_dirty_records(stderr: &str) -> Vec<crate::build_log::FingerprintDirty> {
    let mut out = Vec::new();
    let mut lines = stderr.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line
            .find(DIRTY_RECORD_START)
            .map(|i| &line[i + DIRTY_RECORD_START.len()..])
        else {
            continue;
        };
        // `<name> v<version>/<mode>/<target debug>`: the name has no spaces;
        // the version runs to the first `/`.
        let mut words = rest.splitn(2, ' ');
        let name = words.next().unwrap_or_default();
        let version = words
            .next()
            .and_then(|v| v.strip_prefix('v'))
            .map(|v| v.split('/').next().unwrap_or(v))
            .unwrap_or_default();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        let reason = match lines.peek() {
            Some(next) if next.contains(DIRTY_RECORD_REASON) => {
                let next = lines.next().unwrap_or_default();
                next.find(DIRTY_RECORD_REASON)
                    .map(|i| next[i + DIRTY_RECORD_REASON.len()..].trim().to_string())
                    .unwrap_or_default()
            }
            _ => String::new(),
        };
        out.push(crate::build_log::FingerprintDirty {
            name: name.to_string(),
            version: version.to_string(),
            reason,
        });
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

impl<W: Write> FingerprintNoiseFilter<W> {
    /// Emit whatever is buffered as a final line. Call once at EOF: a
    /// partial last line is real output (a prompt, a progress fragment),
    /// but only a complete line can be a filtered record, so it must not
    /// be released on every `flush()` -- the pipe reader flushes after each
    /// 8 KiB read, and releasing the fragment there re-glued cargo's
    /// records around the chunk boundary (soldr#3099).
    pub(crate) fn finish(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.inner.write_all(&line)?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for FingerprintNoiseFilter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut rest = buf;
        // `\r` ends a line too: cargo's progress redraws never contain a
        // fingerprint record, and holding them until `\n` would freeze an
        // interactive progress bar.
        while let Some(pos) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            let (head, tail) = rest.split_at(pos + 1);
            let line = if self.pending.is_empty() {
                head.to_vec()
            } else {
                let mut line = std::mem::take(&mut self.pending);
                line.extend_from_slice(head);
                line
            };
            self.emit_line(&line)?;
            rest = tail;
        }
        self.pending.extend_from_slice(rest);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Only the inner writer: the pending fragment stays until its line
        // terminator or `finish()`.
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(chunks: &[&str]) -> String {
        let mut out = Vec::new();
        {
            let mut filter = FingerprintNoiseFilter::new(&mut out);
            for chunk in chunks {
                filter.write_all(chunk.as_bytes()).expect("write");
                filter.flush().expect("flush");
            }
            filter.finish().expect("finish");
        }
        String::from_utf8(out).expect("utf8")
    }

    const COLD_MISS: &str = concat!(
        "    0.135975793s  INFO prepare_target{force=false package_id=regex v1.13.1 target=\"regex\"}: cargo::core::compiler::fingerprint: fingerprint error for regex v1.13.1/Build/TargetInner { ..: lib_target(\"regex\", [\"lib\"], \"/x/regex-1.13.1/src/lib.rs\", Edition2021) }\n",
        "    0.135986864s  INFO prepare_target{force=false package_id=regex v1.13.1 target=\"regex\"}: cargo::core::compiler::fingerprint:     err: failed to read `/t/debug/.fingerprint/regex-7bd/lib-regex`\n",
        "\n",
        "Caused by:\n",
        "    No such file or directory (os error 2)\n",
    );

    const DIRTY: &str = concat!(
        "    0.2s  INFO prepare_target{force=false package_id=serde v1.0.0 target=\"serde\"}: cargo::core::compiler::fingerprint: fingerprint dirty for serde v1.0.0/Build/TargetInner { .. }\n",
        "    0.2s  INFO prepare_target{force=false package_id=serde v1.0.0 target=\"serde\"}: cargo::core::compiler::fingerprint:     dirty: the config settings changed\n",
    );

    #[test]
    fn cold_miss_records_are_dropped_and_dirty_records_survive() {
        let input = format!(
            "   Compiling libc v0.2.189\n{COLD_MISS}{DIRTY}{COLD_MISS}soldr[cache] libc [MISS]\n"
        );
        let expected = format!("   Compiling libc v0.2.189\n{DIRTY}soldr[cache] libc [MISS]\n");
        assert_eq!(filtered(&[&input]), expected);
    }

    #[test]
    fn a_record_split_across_chunks_is_still_dropped() {
        let input = format!("before\n{COLD_MISS}after\n");
        let mid = input.len() / 2;
        let (a, b) = input.split_at(mid);
        assert_eq!(filtered(&[a, b]), "before\nafter\n");
        let pieces: Vec<String> = input
            .as_bytes()
            .chunks(7)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        assert_eq!(filtered(&refs), "before\nafter\n");
    }

    #[test]
    fn backtrace_frames_under_a_record_are_dropped_too() {
        let input = format!(
            "{}Stack backtrace:\n   0: cargo_util::paths::read_bytes\n             at /rustc/x/lib.rs:1:1\n  29: __libc_start_main\nerror: real failure\n",
            COLD_MISS
        );
        assert_eq!(filtered(&[&input]), "error: real failure\n");
    }

    #[test]
    fn unrelated_indented_and_blank_lines_pass_through() {
        let input = "warning: unused variable\n  --> src/main.rs:1:5\n\n   = note: x\n";
        assert_eq!(filtered(&[input]), input);
    }

    #[test]
    fn dirty_records_are_extracted_with_their_reason() {
        let input = format!("noise\n{COLD_MISS}{DIRTY}    0.3s  INFO prepare_target{{..}}: cargo::core::compiler::fingerprint: fingerprint dirty for soldr-cli v0.9.12/Build/TargetInner {{ .. }}\n   Compiling x\n");
        let got = extract_dirty_records(&input);
        assert_eq!(
            got,
            vec![
                crate::build_log::FingerprintDirty {
                    name: "serde".into(),
                    version: "1.0.0".into(),
                    reason: "the config settings changed".into(),
                },
                crate::build_log::FingerprintDirty {
                    name: "soldr-cli".into(),
                    version: "0.9.12".into(),
                    reason: String::new(),
                },
            ]
        );
    }

    #[test]
    fn a_partial_last_line_is_released_at_finish_only() {
        assert_eq!(filtered(&["progress 50%"]), "progress 50%");
        let mut out = Vec::new();
        {
            let mut filter = FingerprintNoiseFilter::new(&mut out);
            filter.write_all(b"half a").expect("write");
            filter.flush().expect("flush");
            assert!(
                out_is_empty(&filter),
                "flush must not release a partial line"
            );
            filter.finish().expect("finish");
        }
        assert_eq!(String::from_utf8(out).expect("utf8"), "half a");
    }

    fn out_is_empty<W: Write>(filter: &FingerprintNoiseFilter<W>) -> bool {
        !filter.pending.is_empty()
    }

    #[test]
    fn a_record_split_by_a_chunk_flush_is_still_dropped() {
        // The pipe reader flushes after every read; a flush inside a record
        // must not re-emit the fragment (soldr#3099's real leak).
        let input = format!("before\n{COLD_MISS}after\n");
        for size in [3usize, 7, 64, 1000] {
            let pieces: Vec<String> = input
                .as_bytes()
                .chunks(size)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
            assert_eq!(filtered(&refs), "before\nafter\n", "chunk size {size}");
        }
    }

    #[test]
    fn carriage_return_redraws_pass_through_promptly() {
        let mut out = Vec::new();
        {
            let mut filter = FingerprintNoiseFilter::new(&mut out);
            filter.write_all(b"Building [==>  ] 1/3\r").expect("write");
            assert_eq!(out_len(&filter), 0, "pending must be empty after a \\r");
            filter.finish().expect("finish");
        }
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "Building [==>  ] 1/3\r"
        );
    }

    fn out_len<W: Write>(filter: &FingerprintNoiseFilter<W>) -> usize {
        filter.pending.len()
    }
}
