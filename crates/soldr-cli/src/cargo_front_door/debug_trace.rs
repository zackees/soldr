//! soldr#2546 — opt-in build process tracing (`soldr --debug cargo build`).
//!
//! First slice of the recursive-tree debugging surface: every child soldr
//! itself spawns on the cargo front door is announced on stderr with elapsed
//! time and PID, and appended as a JSONL event beside the build logs
//! (`<soldr root>/logs/debug-trace/<epoch-ms>-<pid>.jsonl`). Descendant
//! (grandchild) observation arrives with the running-process
//! `with_observer_and_command` seam (running-process#1023) in a later slice;
//! the JSONL schema here is the timeline that observer enriches.
//!
//! Off by default: with the flag absent, [`enabled`] is a single env read and
//! no file, thread, or buffer is created. The `--debug` flag publishes
//! [`DEBUG_TRACE_ENV_VAR`] (see `Cli::export_global_env`) so nested soldr
//! invocations inherit tracing.

use std::io::Write;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub(crate) const DEBUG_TRACE_ENV_VAR: &str = "SOLDR_DEBUG_TRACE";

pub(crate) fn enabled() -> bool {
    match std::env::var(DEBUG_TRACE_ENV_VAR) {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !(value.is_empty() || matches!(value.as_str(), "0" | "false" | "no" | "off"))
        }
        Err(_) => false,
    }
}

/// Spawn `command`, emitting a `spawned` trace event when tracing is on.
///
/// The error type stays `std::io::Error` so call sites keep their existing
/// context-specific error mapping.
pub(crate) fn spawn_traced(command: &mut Command, context: &str) -> std::io::Result<Child> {
    if !enabled() {
        return command.spawn();
    }
    let argv = render_argv(command);
    let child = command.spawn()?;
    emit(
        context,
        &format!("spawned pid={} ({context}): {argv}", child.id()),
        &format!(
            r#"{{"event":"spawned","t_ms":{},"pid":{},"context":{},"argv":{}}}"#,
            elapsed_ms(),
            child.id(),
            json_string(context),
            json_string(&argv),
        ),
    );
    Ok(child)
}

/// Emit an `exited` trace event. Call after a traced child has been reaped.
pub(crate) fn child_exited(pid: u32, context: &str, status: &ExitStatus) {
    if !enabled() {
        return;
    }
    let code = status.code();
    let rendered = code.map_or_else(|| "signal".to_string(), |code| code.to_string());
    emit(
        context,
        &format!("exited pid={pid} ({context}) code={rendered}"),
        &format!(
            r#"{{"event":"exited","t_ms":{},"pid":{},"context":{},"exit_code":{}}}"#,
            elapsed_ms(),
            pid,
            json_string(context),
            code.map_or_else(|| "null".to_string(), |code| code.to_string()),
        ),
    );
}

fn render_argv(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    parts.join(" ")
}

/// Minimal JSON string encoder for the trace lines. The values are
/// best-effort lossy renderings already; a full serializer dependency is
/// not warranted for two string fields.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn trace_epoch() -> &'static Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now)
}

fn elapsed_ms() -> u128 {
    trace_epoch().elapsed().as_millis()
}

fn elapsed_secs_display() -> String {
    format!("{:.2}", trace_epoch().elapsed().as_secs_f64())
}

/// The JSONL sink, created lazily on the first traced event. `None` when the
/// file could not be created — tracing then degrades to stderr-only rather
/// than failing the build.
fn sink() -> &'static Option<Mutex<std::fs::File>> {
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    SINK.get_or_init(|| {
        let dir = match crate::core::SoldrPaths::new() {
            Ok(paths) => paths.root.join("logs").join("debug-trace"),
            Err(_) => return None,
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("{now_ms}-{}.jsonl", std::process::id()));
        match std::fs::File::create(&path) {
            Ok(mut file) => {
                let _ = writeln!(
                    file,
                    r#"{{"event":"anchor","epoch_ms":{now_ms},"soldr_pid":{}}}"#,
                    std::process::id()
                );
                eprintln!("soldr debug: process timeline -> {}", path.display());
                Some(Mutex::new(file))
            }
            Err(_) => None,
        }
    })
}

fn emit(_context: &str, human: &str, json: &str) {
    eprintln!("soldr debug: [+{}s] {human}", elapsed_secs_display());
    if let Some(file) = sink() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{json}");
        }
    }
}

#[cfg(test)]
#[path = "debug_trace_tests.rs"]
mod tests;
