//! `RUSTC_WRAPPER` invocation path: forwards rustc / clippy-driver through
//! zccache, spills stdin to a temp file when cargo passes `-`, and recovers
//! from "unknown session" errors on Windows. Extracted from `main.rs` as
//! part of issue #339.

use crate::zccache::run_zccache_command_in_cache_dir;
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary, zccache_binary_override};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};

/// Known toolchain binaries that cargo may invoke through RUSTC_WRAPPER
/// or RUSTC_WORKSPACE_WRAPPER. When soldr is set as a wrapper, cargo
/// passes: `soldr <toolchain-binary> <rustc-args...>`
const WRAPPER_PASSTHROUGH_TOOLS: &[&str] = &["rustc", "clippy-driver"];

pub(crate) fn is_wrapper_invocation(arg: &str) -> bool {
    let stem = std::path::Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);

    WRAPPER_PASSTHROUGH_TOOLS.contains(&stem)
}

pub(crate) fn run_rustc_wrapper(raw_args: &[String]) -> Result<i32, SoldrError> {
    let tool_arg = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing tool path in wrapper mode".into()))?;

    let tool_stem = std::path::Path::new(tool_arg.as_str())
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(tool_arg);

    // Per-build target/ tracking for `soldr gc`. Best-effort: if we
    // can't resolve a workspace target dir cheaply, or the redb
    // upsert fails for any reason, skip silently — never fail a build.
    if tool_stem == "rustc" {
        record_target_dir_in_registry(&raw_args[2..]);
    }

    // When the source argument is "-" (stdin), rustc reads the source from
    // the process's stdin. If we pass this invocation to zccache as-is,
    // zccache reads stdin to hash the source content, exhausting the pipe
    // before rustc is spawned. Rustc then receives an empty stdin, compiles
    // nothing, and exits 0 — masking any real compile error (e.g. E0554 from
    // build-script feature probes like rustix 0.37's `can_compile()`).
    //
    // Fix: spill stdin to a temp file so both zccache and rustc see a real
    // path. The temp file is created in the system temp directory and removed
    // after the child exits. This keeps zccache in the loop (it can hash the
    // file normally) while preserving the correct exit code.
    let stdin_tempfile = if raw_args[2..].iter().any(|a| a == "-") {
        Some(spill_stdin_to_tempfile()?)
    } else {
        None
    };

    // Build the effective arg list, replacing "-" with the temp file path.
    let effective_args: std::borrow::Cow<[String]> = if let Some(ref tmp) = stdin_tempfile {
        let tmp_str = tmp.path().to_string_lossy().into_owned();
        let replaced: Vec<String> = raw_args
            .iter()
            .cloned()
            .map(|a| if a == "-" { tmp_str.clone() } else { a })
            .collect();
        std::borrow::Cow::Owned(replaced)
    } else {
        std::borrow::Cow::Borrowed(raw_args)
    };

    // Only route through zccache for actual rustc invocations, not
    // clippy-driver or other workspace wrappers.
    if tool_stem == "rustc" && soldr_cache::cache_enabled_in_current_process() {
        if let Some(zccache) = zccache_binary_override() {
            return run_wrapper_through_zccache(&effective_args, &zccache);
        }
    }

    // Resolve the tool binary. If it's already a full path, use it
    // directly. Otherwise resolve via rustup.
    let tool_path: std::path::PathBuf = if std::path::Path::new(tool_arg.as_str()).is_absolute() {
        tool_arg.into()
    } else {
        resolve_toolchain_binary(tool_stem)?
    };

    let mut command = std::process::Command::new(tool_path);
    command.args(&effective_args[2..]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

/// Read all of stdin into a named temporary file and return the file.
///
/// The file has a `.rs` extension so rustc accepts it without flags, and
/// lives in the system temp directory. It is deleted when the returned
/// `NamedTempFile` value is dropped (i.e. after the child process exits).
fn spill_stdin_to_tempfile() -> Result<tempfile::NamedTempFile, SoldrError> {
    use std::io::{Read, Write as _};
    let mut tmp = tempfile::Builder::new()
        .prefix("soldr-stdin-")
        .suffix(".rs")
        .tempfile()
        .map_err(|e| SoldrError::Other(format!("failed to create stdin temp file: {e}")))?;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SoldrError::Other(format!("failed to read stdin: {e}")))?;
    tmp.write_all(&buf)
        .map_err(|e| SoldrError::Other(format!("failed to write stdin temp file: {e}")))?;
    Ok(tmp)
}

fn run_wrapper_through_zccache(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(zccache);
    command.args(&raw_args[1..]);
    suppress_windows_console_window(&mut command);

    // Cargo's jobserver lives on numbered file descriptors that it inherits
    // into the RUSTC_WRAPPER, advertised via CARGO_MAKEFLAGS. On Unix,
    // exec'ing into zccache replaces the wrapper process in-place so those
    // FDs flow straight through to the inner rustc — rustc otherwise emits
    // "failed to connect to jobserver from environment variable
    // CARGO_MAKEFLAGS=...: cannot open file descriptor N" because spawning
    // a Rust child closes any FDs not explicitly inherited.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // TODO(#265): once exec() replaces this process there is nowhere to
        // observe zccache's stderr or retry on "unknown session:". The
        // Windows branch below performs that defensive retry. A Unix port
        // would need to spawn-with-piped-stderr instead of exec, while still
        // forwarding the cargo jobserver FDs (see issue #265 for context).
        let err = command.exec();
        Err(SoldrError::Other(format!(
            "failed to exec zccache at {}: {err}",
            zccache.display()
        )))
    }

    #[cfg(not(unix))]
    {
        run_wrapper_through_zccache_windows(raw_args, zccache)
    }
}

/// Windows-only wrapper invocation: spawn zccache with its stderr piped so we
/// can tee it to our own stderr live AND scan it after the process exits.
///
/// If zccache returns a non-zero exit and its stderr contains the literal
/// substring `unknown session:` (issue #265), the managed zccache daemon was
/// killed mid-build by something outside soldr's control (e.g. zccache-ci's
/// stop hook on older zccache, AV quarantine, or a Windows binary
/// replacement). We allocate a fresh session via `zccache session-start` and
/// retry the wrapper invocation exactly once with the new session id.
///
/// Retry budget is 1. On the retry's own failure we propagate that exit code
/// unchanged — we don't loop on a persistently broken daemon.
#[cfg(not(unix))]
fn run_wrapper_through_zccache_windows(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    use std::io::Read;
    use std::process::Stdio;

    let mut command = std::process::Command::new(zccache);
    command.args(&raw_args[1..]);
    command.stderr(Stdio::piped());
    suppress_windows_console_window(&mut command);

    let mut child = command.spawn()?;
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as piped above");

    // Tee zccache stderr to soldr's stderr in real time AND buffer it for
    // post-exit inspection. A reader thread keeps the pipe drained so
    // zccache cannot block on a full pipe.
    let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(stderr);
        let mut chunk = [0u8; 4096];
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            // Best-effort tee: if writing to our own stderr fails we still
            // want to keep draining the child pipe.
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &chunk[..n]);
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok(buf)
    });

    let status = child.wait()?;
    let stderr_bytes = reader
        .join()
        .map_err(|_| SoldrError::Other("zccache stderr reader thread panicked".into()))?
        .unwrap_or_default();

    let exit_code = status.code().unwrap_or(1);
    if status.success() || !stderr_indicates_unknown_session(&stderr_bytes) {
        return Ok(exit_code);
    }

    // Daemon told us our session id is gone. Allocate a fresh one and
    // retry the wrapper invocation once.
    let new_session_id = match allocate_replacement_session(zccache) {
        Ok(id) => id,
        Err(err) => {
            eprintln!(
                "soldr: zccache reported \"unknown session:\" but soldr could not allocate \
                 a replacement session ({err}); propagating original exit code"
            );
            return Ok(exit_code);
        }
    };

    eprintln!(
        "soldr: zccache session resync after \"unknown session:\"; retrying once with fresh session {new_session_id}"
    );

    let mut retry = std::process::Command::new(zccache);
    retry.args(&raw_args[1..]);
    retry.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &new_session_id);
    suppress_windows_console_window(&mut retry);
    let retry_status = retry.status()?;
    Ok(retry_status.code().unwrap_or(1))
}

/// Returns `true` iff `stderr` contains the literal substring
/// `unknown session:` somewhere in its bytes. Tolerates non-UTF-8 input.
///
/// Extracted as a pure helper so the retry trigger can be unit-tested
/// without spawning a real zccache.
#[cfg_attr(unix, allow(dead_code))]
pub(crate) fn stderr_indicates_unknown_session(stderr: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"unknown session:";
    if stderr.len() < NEEDLE.len() {
        return false;
    }
    stderr.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

/// Run `zccache session-start --stats --log <path> --journal <path>` against
/// the cache dir the wrapper invocation inherits from cargo, and return the
/// parsed session id. Mirrors the args used by `prepare_zccache_build`.
///
/// Used by the Windows wrapper retry path (issue #265): when the daemon
/// reports `unknown session:` the in-process session id is stale, so soldr
/// allocates a replacement before retrying the wrapper invocation once.
#[cfg(not(unix))]
fn allocate_replacement_session(zccache: &std::path::Path) -> Result<String, SoldrError> {
    let cache_dir = std::env::var_os(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "{} is not set in the wrapper environment; cannot allocate replacement zccache session",
                soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR
            ))
        })?;

    let session_log_path = soldr_cache::session_log_path(&cache_dir);
    let session_log_path_arg = session_log_path.display().to_string();
    let journal_path = soldr_cache::session_journal_path(&cache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_json = run_zccache_command_in_cache_dir(
        zccache,
        &[
            "session-start",
            "--stats",
            "--log",
            &session_log_path_arg,
            "--journal",
            &journal_path_arg,
        ],
        &cache_dir,
    )?;
    soldr_cache::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to parse zccache session id from output: {}",
            session_json.stdout.trim()
        ))
    })
}

/// Best-effort upsert of the workspace `target/` dir into the soldr
/// state registry on every wrapper invocation. Silent on failure.
///
/// `rustc_args` is the slice of args that follows the rustc binary
/// path in the wrapper invocation (i.e. `raw_args[2..]`).
fn record_target_dir_in_registry(rustc_args: &[String]) {
    let Some(target) = soldr_cache::target_registry::resolve_workspace_target_dir(rustc_args)
    else {
        return;
    };
    let Ok(paths) = SoldrPaths::new() else { return };
    let db_path = soldr_cache::data_db_path(&paths);
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return;
    };
    let _ = registry.upsert(&target);
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- stderr_indicates_unknown_session (issue #265) --------

    #[test]
    fn unknown_session_detector_rejects_empty_stderr() {
        assert!(!stderr_indicates_unknown_session(b""));
    }

    #[test]
    fn unknown_session_detector_matches_exact_zccache_line() {
        let stderr = b"zccache error: unknown session: abc-123\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_matches_substring_mid_line() {
        // The marker can appear anywhere in the stream, not necessarily at
        // the start of a line.
        let stderr = b"prelude blah blah unknown session: 0000 trailing\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_ignores_unrelated_session_mentions() {
        // The word "session" alone is not enough; we only treat the literal
        // "unknown session:" marker as a resync trigger.
        let stderr = b"zccache info: session started\nzccache info: session ok\n";
        assert!(!stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_tolerates_non_utf8_bytes() {
        // Surround the marker with raw non-UTF-8 byte sequences; the
        // detector must not panic and must still find the literal needle.
        let mut stderr: Vec<u8> = vec![0xFF, 0xFE, 0x80, 0x81];
        stderr.extend_from_slice(b"zccache error: unknown session: deadbeef\n");
        stderr.extend_from_slice(&[0xC3, 0x28, 0xA0]);
        assert!(stderr_indicates_unknown_session(&stderr));
    }

    #[test]
    fn unknown_session_detector_rejects_partial_marker() {
        // "unknown sessio" (missing the trailing "n:") must NOT match — we
        // only resync on the exact daemon-emitted marker.
        let stderr = b"unknown sessio\n";
        assert!(!stderr_indicates_unknown_session(stderr));
    }
}
