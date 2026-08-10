//! Shared broker-SESSION compile dispatch.
//!
//! Both the soldr `RUSTC_WRAPPER` entry and the `zccache-soldr` multicall
//! entry terminate here. The broker is the only daemon-acquisition front
//! door; this module has no direct daemon socket, spawn, or rustc fallback.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::core::{SoldrError, SoldrPaths};
use crate::daemon::protocol::CompileRequest;

/// Build the wire request used by wrapper-performance and env-contract tests.
pub fn build_compile_request(rustc_argv: &[String]) -> CompileRequest {
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    crate::daemon::compile_request::build_compile_request_from(rustc_argv, cwd, std::env::vars())
}

/// Re-export the shared environment predicate for existing contract tests.
pub use crate::daemon::compile_request::is_compile_env_var;

/// Historical fallback journal path retained for status/doctor compatibility.
///
/// The mandatory broker cutover does not append new records. Existing records
/// remain readable so upgrading soldr does not erase diagnostic history.
pub(crate) fn compile_daemon_fallback_log_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .root
        .join("logs")
        .join("compile-daemon-fallbacks.jsonl")
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompileFallbackCursor {
    len: u64,
    tail_anchor: Vec<u8>,
}

/// Capture a cheap cursor for the historical fallback journal.
pub(crate) fn compile_daemon_fallback_cursor(paths: &SoldrPaths) -> CompileFallbackCursor {
    const ANCHOR_BYTES: u64 = 512;

    let path = compile_daemon_fallback_log_path(paths);
    let Ok(mut file) = std::fs::File::open(path) else {
        return CompileFallbackCursor::default();
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return CompileFallbackCursor::default();
    };
    let anchor_start = len.saturating_sub(ANCHOR_BYTES);
    if file.seek(SeekFrom::Start(anchor_start)).is_err() {
        return CompileFallbackCursor::default();
    }
    let mut tail_anchor = Vec::with_capacity((len - anchor_start) as usize);
    if file.read_to_end(&mut tail_anchor).is_err() {
        return CompileFallbackCursor::default();
    }
    CompileFallbackCursor { len, tail_anchor }
}

/// Count historical fallback records appended during a front-door session.
///
/// This normally returns zero after the mandatory broker cutover. Keeping the
/// reader avoids changing the existing status/log JSON shape in the same PR.
pub(crate) fn compile_daemon_fallback_count_since(
    paths: &SoldrPaths,
    cursor: &CompileFallbackCursor,
    session_id: u64,
) -> std::io::Result<(usize, PathBuf)> {
    let path = compile_daemon_fallback_log_path(paths);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, path)),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    let anchor_start = cursor.len.saturating_sub(cursor.tail_anchor.len() as u64);
    let anchor_matches = if len < cursor.len {
        false
    } else {
        file.seek(SeekFrom::Start(anchor_start))?;
        let mut current_anchor = vec![0; cursor.tail_anchor.len()];
        file.read_exact(&mut current_anchor)?;
        current_anchor == cursor.tail_anchor
    };
    file.seek(SeekFrom::Start(if anchor_matches { cursor.len } else { 0 }))?;
    let mut appended = String::new();
    file.read_to_string(&mut appended)?;
    let mut count = 0;
    for (index, line) in appended.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "malformed fallback record {} in {}: {error}",
                    index + 1,
                    path.display()
                ),
            )
        })?;
        if event["event"] == "compile_daemon_fallback"
            && event["session_id"].as_u64() == Some(session_id)
        {
            count += 1;
        }
    }
    Ok((count, path))
}

/// Dispatch one rustc-style invocation through the broker SESSION service.
pub fn dispatch_compile<O, E>(
    rustc_argv: &[String],
    _stdout: O,
    _stderr: E,
) -> Result<i32, SoldrError>
where
    O: Write,
    E: Write,
{
    match crate::session_transport::session_hot_path(rustc_argv) {
        crate::session_transport::SessionHotPathOutcome::Served(exit_code) => Ok(exit_code),
        crate::session_transport::SessionHotPathOutcome::HardFail(error) => Err(SoldrError::Other(
            format!("broker SESSION compile failed: {error}"),
        )),
    }
}

/// Wrapper/multicall convenience entry using the process standard streams.
pub fn compile_via_daemon(rustc_argv: &[String]) -> Result<i32, SoldrError> {
    dispatch_compile(rustc_argv, std::io::stdout(), std::io::stderr())
}
