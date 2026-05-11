//! Fake zccache binary used by `tests/cli_unknown_session_retry.rs` for #266.
//!
//! Models three stub modes:
//!
//! * `session-start` invocation: emit a JSON object on stdout that
//!   `soldr_cache::parse_zccache_session_id` accepts, and exit 0. Also
//!   records the invocation in the state file under `session_starts`.
//! * Wrapper-style invocation (first positional arg is not a known
//!   zccache subcommand): consult the state file to decide whether to
//!   emit `zccache error: unknown session: <id>\n` to stderr and exit
//!   1, or exit 0 silently. Always records the invocation in the state
//!   file under `wrapper_calls`.
//!
//! The state file is JSON, written atomically via overwrite. Path is
//! provided by the test via `FAKE_ZCCACHE_STATE_FILE`. The mode is
//! provided via `FAKE_ZCCACHE_MODE`:
//!
//! * `fail_first` (default): only the first wrapper call emits
//!   unknown-session; later calls succeed.
//! * `always_fail`: every wrapper call emits unknown-session.
//! * `non_session_failure`: every wrapper call emits an unrelated
//!   stderr message and exits 1.
//!
//! No external dependencies — `std` only.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_ENV_VAR: &str = "FAKE_ZCCACHE_STATE_FILE";
const MODE_ENV_VAR: &str = "FAKE_ZCCACHE_MODE";
const SESSION_ID_ENV_VAR: &str = "ZCCACHE_SESSION_ID";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    // session-start: a recognised zccache subcommand. The retry path
    // calls `zccache session-start --stats --log <path> --journal <path>`
    // in the cache dir. Mirror that JSON shape.
    if args.iter().any(|a| a == "session-start") {
        return handle_session_start();
    }

    // Any other invocation with an arg is treated as wrapper-style.
    if args.is_empty() {
        eprintln!("fake-zccache: no args");
        return ExitCode::from(2);
    }

    handle_wrapper_call()
}

fn handle_session_start() -> ExitCode {
    record_event("session_starts");

    let counter = read_counter("session_starts");
    let session_id = format!("retry-uuid-{counter}");
    println!(
        "{{\"session_id\":\"{session_id}\",\"started_at\":{}}}",
        now_secs()
    );
    ExitCode::SUCCESS
}

fn handle_wrapper_call() -> ExitCode {
    let prior_wrapper_calls = read_counter("wrapper_calls");
    record_event("wrapper_calls");

    let mode = env::var(MODE_ENV_VAR).unwrap_or_else(|_| "fail_first".to_string());
    match mode.as_str() {
        "always_fail" => emit_unknown_session(),
        "non_session_failure" => {
            eprintln!("fake-zccache: compile error: simulated unrelated failure");
            ExitCode::from(1)
        }
        // Default: only the first call fails.
        _ => {
            if prior_wrapper_calls == 0 {
                emit_unknown_session()
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}

fn emit_unknown_session() -> ExitCode {
    let session_id = env::var(SESSION_ID_ENV_VAR).unwrap_or_else(|_| "<no-session>".to_string());
    eprintln!("zccache error: unknown session: {session_id}");
    ExitCode::from(1)
}

fn state_path() -> Option<PathBuf> {
    env::var_os(STATE_ENV_VAR).map(PathBuf::from)
}

/// Record an event by name, e.g. `session_starts` or `wrapper_calls`.
/// Stored as a tiny JSON-ish file with `key=<count>` lines. Atomic
/// enough for single-threaded test usage: read, increment, overwrite.
fn record_event(key: &str) {
    let Some(path) = state_path() else { return };
    let mut counts = load_counts(&path);
    let entry = counts.iter_mut().find(|(k, _)| k == key);
    match entry {
        Some((_, n)) => *n += 1,
        None => counts.push((key.to_string(), 1)),
    }
    let _ = write_counts(&path, &counts);
}

fn read_counter(key: &str) -> u64 {
    let Some(path) = state_path() else { return 0 };
    load_counts(&path)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or(0)
}

fn load_counts(path: &Path) -> Vec<(String, u64)> {
    let Ok(body) = fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let v: u64 = v.trim().parse().ok()?;
            Some((k.trim().to_string(), v))
        })
        .collect()
}

fn write_counts(path: &Path, counts: &[(String, u64)]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for (k, v) in counts {
        body.push_str(&format!("{k}={v}\n"));
    }
    let mut file = fs::File::create(path)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
