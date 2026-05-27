//! `RUSTC_WRAPPER` invocation path: forwards rustc / clippy-driver through
//! zccache, spills stdin to a temp file when cargo passes `-`, and recovers
//! from "unknown session" errors on Windows. Extracted from `main.rs` as
//! part of issue #339.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::startup_profile::WrapperProfile;
#[cfg(not(unix))]
use crate::zccache_lifecycle::{
    session_start_args, stderr_indicates_unknown_session, ZccacheLifecycle,
    ZccachePrivateDaemonConfig, ZccachePrivateEnv, ZccacheSessionStartOptions,
    ZCCACHE_DAEMON_NAMESPACE_ENV_VAR,
};
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary, zccache_binary_override};

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

pub(crate) fn run_rustc_wrapper(
    raw_args: &[String],
    mut profile: WrapperProfile,
) -> Result<i32, SoldrError> {
    let tool_arg = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing tool path in wrapper mode".into()))?;

    let tool_stem = std::path::Path::new(tool_arg.as_str())
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(tool_arg);

    profile.mark("tool_resolved");

    // Per-build target/ tracking for `soldr gc`. Best-effort: if we
    // can't resolve a workspace target dir cheaply, or the redb
    // upsert fails for any reason, skip silently — never fail a build.
    //
    // The phase emitted afterwards tells SOLDR_PROFILE_STARTUP=1
    // consumers which routing path fired — explicitly distinguishing
    // the daemon path from the fast direct-redb path proves the
    // Option-A invariant from #474: outside a soldr-cargo session,
    // no `daemon`/`is_live`/`socket`/`record_target_touch_or_fallback`
    // phase appears in the profile.
    if tool_stem == "rustc" {
        let path = record_target_dir_in_registry(&raw_args[2..]);
        let mark = match path {
            TargetTouchPath::NoTarget => "target_dir_recorded_no_target",
            TargetTouchPath::NoPaths => "target_dir_recorded_no_paths",
            TargetTouchPath::FastDirect => "target_dir_recorded_fast",
            TargetTouchPath::DaemonFirst => "target_dir_recorded_daemon",
            TargetTouchPath::MemoSkipped => "target_dir_recorded_memo",
        };
        profile.mark(mark);
    } else {
        profile.mark("target_dir_recorded");
    }

    // When the source argument is "-" (stdin), rustc reads the source from
    // the process's stdin. If we pass this invocation to zccache as-is,
    // zccache reads stdin to hash the source content, exhausting the pipe
    // before rustc is spawned. Rustc then receives an empty stdin, compiles
    // nothing, and exits 0 — masking any real compile error (e.g. E0554 from
    // build-script feature probes like rustix 0.37's `can_compile()`).
    //
    // Fix: spill stdin to a content-addressed temp file so both zccache and
    // rustc see a stable real path. This keeps zccache in the loop (it can
    // hash the file normally) while preserving the correct exit code, and it
    // lets identical feature probes converge on the same cache key.
    let stdin_tempfile = if raw_args[2..].iter().any(|a| a == "-") {
        Some(spill_stdin_to_content_addressed_file()?)
    } else {
        None
    };
    profile.mark("stdin_handled");

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
    if tool_stem == "rustc" && crate::cache_lib::cache_enabled_in_current_process() {
        if let Some(zccache) = zccache_binary_override() {
            profile.mark("zccache_resolved");
            // On Unix `run_wrapper_through_zccache` exec()'s and never
            // returns, so the profile MUST emit before that — finish()
            // is the last in-process work we can attribute.
            profile.finish("before_zccache_exec");
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
    profile.finish("before_tool_spawn");
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

struct StdinSourceFile {
    path: std::path::PathBuf,
}

impl StdinSourceFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Read all of stdin into a content-addressed source file and return it.
///
/// The file has a `.rs` extension so rustc accepts it without flags, and
/// lives in the system temp directory as `soldr-stdin-<short_blake3>.rs`.
/// It is intentionally retained so concurrent identical probes can share the
/// same stable path.
fn spill_stdin_to_content_addressed_file() -> Result<StdinSourceFile, SoldrError> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SoldrError::Other(format!("failed to read stdin: {e}")))?;
    materialize_stdin_source(&buf)
}

fn materialize_stdin_source(bytes: &[u8]) -> Result<StdinSourceFile, SoldrError> {
    let hash = blake3::hash(bytes);
    let hex = hash.to_hex();
    let temp_dir = std::env::temp_dir();
    let short_path = temp_dir.join(format!("soldr-stdin-{}.rs", &hex[..16]));
    if ensure_stdin_source_path(&short_path, bytes)? {
        return Ok(StdinSourceFile { path: short_path });
    }

    let full_path = temp_dir.join(format!("soldr-stdin-{hex}.rs"));
    if ensure_stdin_source_path(&full_path, bytes)? {
        return Ok(StdinSourceFile { path: full_path });
    }

    Err(SoldrError::Other(format!(
        "stdin temp path hash collision at {}",
        full_path.display()
    )))
}

fn ensure_stdin_source_path(path: &std::path::Path, bytes: &[u8]) -> Result<bool, SoldrError> {
    use std::io::Write as _;

    match std::fs::read(path) {
        Ok(existing) => return Ok(existing == bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(SoldrError::Other(format!(
                "failed to read existing stdin temp file {}: {err}",
                path.display()
            )));
        }
    }

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        SoldrError::Other(format!(
            "failed to create stdin temp file in {}: {e}",
            parent.display()
        ))
    })?;
    tmp.write_all(bytes)
        .map_err(|e| SoldrError::Other(format!("failed to write stdin temp file: {e}")))?;
    let _ = tmp.as_file().sync_all();

    match tmp.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = err.file.close();
            let existing = std::fs::read(path).map_err(|e| {
                SoldrError::Other(format!(
                    "failed to read raced stdin temp file {}: {e}",
                    path.display()
                ))
            })?;
            Ok(existing == bytes)
        }
        Err(err) => Err(SoldrError::Other(format!(
            "failed to publish stdin temp file {}: {}",
            path.display(),
            err.error
        ))),
    }
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
    retry.env(
        crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR,
        &new_session_id,
    );
    suppress_windows_console_window(&mut retry);
    let retry_status = retry.status()?;
    Ok(retry_status.code().unwrap_or(1))
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
    let cache_dir = std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "{} is not set in the wrapper environment; cannot allocate replacement zccache session",
                crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR
            ))
        })?;

    let session_log_path = crate::cache_lib::session_log_path(&cache_dir);
    let journal_path = crate::cache_lib::session_journal_path(&cache_dir);
    let session_stats_path = crate::cache_lib::session_stats_path(&cache_dir);
    let options = ZccacheSessionStartOptions {
        id: None,
        session_log_path,
        journal_path,
        session_stats_path,
    };
    let mut lifecycle = ZccacheLifecycle::new(zccache, &cache_dir);
    if let Some(daemon_name) = inherited_private_daemon_name() {
        let private = ZccachePrivateDaemonConfig::new(daemon_name)
            .with_owner_pid(std::process::id())
            .with_private_env(inherited_private_zccache_env());
        lifecycle = lifecycle.with_private_daemon(private);
    }
    let args = session_start_args(&options, &cache_dir, lifecycle.private_daemon());
    let session_json = lifecycle.run_strings(&args)?;
    crate::cache_lib::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to parse zccache session id from output: {}",
            session_json.stdout.trim()
        ))
    })
}

#[cfg(not(unix))]
fn inherited_private_daemon_name() -> Option<String> {
    std::env::var(ZCCACHE_DAEMON_NAMESPACE_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(unix))]
fn inherited_private_zccache_env() -> Vec<ZccachePrivateEnv> {
    [
        crate::cache_lib::ZCCACHE_PATH_REMAP_ENV_VAR,
        crate::cache_lib::ZCCACHE_WORKTREE_ROOT_ENV_VAR,
    ]
    .into_iter()
    .filter_map(|key| {
        let value = std::env::var_os(key)?;
        if value.is_empty() {
            return None;
        }
        Some(ZccachePrivateEnv::new(
            key,
            value.to_string_lossy().to_string(),
        ))
    })
    .collect()
}

// Routing logic + `TargetTouchPath` live in `wrapper_target.rs` so the
// integration tests in `tests/cli_wrapper_perf.rs` can drive
// `record_target_dir_in_registry` in-process via the lib's
// `pub mod wrapper_target;` declaration. Re-exported here so existing
// bin-side call sites in this file keep working unchanged.
pub(crate) use crate::wrapper_target::{record_target_dir_in_registry, TargetTouchPath};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zccache_lifecycle::stderr_indicates_unknown_session;

    #[test]
    fn stdin_source_path_is_content_addressed() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let bytes = format!("fn main() {{ let _ = {nonce}; }}\n");
        let file = materialize_stdin_source(bytes.as_bytes()).unwrap();
        let hash = blake3::hash(bytes.as_bytes()).to_hex();
        let name = file.path().file_name().unwrap().to_string_lossy();

        assert_eq!(name.as_ref(), format!("soldr-stdin-{}.rs", &hash[..16]));
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes.as_bytes());

        let same = materialize_stdin_source(bytes.as_bytes()).unwrap();
        assert_eq!(same.path(), file.path());
    }

    #[test]
    fn stdin_source_paths_do_not_collide_for_distinct_content() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let a = materialize_stdin_source(format!("const A: u128 = {nonce};\n").as_bytes()).unwrap();
        let b = materialize_stdin_source(format!("const B: u128 = {};\n", nonce + 1).as_bytes())
            .unwrap();

        assert_ne!(a.path(), b.path());
    }

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
