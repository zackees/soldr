//! Crash diagnostics for the broker process.
//!
//! This installs a process-wide panic hook early in broker startup so
//! unexpected Rust panics leave a small text crash report even when the
//! process is daemonized or launched by a service manager.

use std::backtrace::Backtrace;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable that overrides the crash report directory.
pub const CRASH_DUMP_DIR_ENV: &str = "RUNNING_PROCESS_BROKER_CRASH_DUMP_DIR";

static INSTALLED: AtomicBool = AtomicBool::new(false);
static CRASH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Errors returned while installing crash diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum CrashDumpError {
    /// Component names become part of crash report filenames.
    #[error(
        "invalid broker crash dump component name {component:?}; use 1-64 ASCII letters, digits, '-' or '_'"
    )]
    InvalidComponent {
        /// Invalid component name supplied by the caller.
        component: String,
    },
    /// The crash report directory could not be created.
    #[error("failed to create crash dump directory {path:?}: {source}")]
    Directory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
}

/// Install the broker crash diagnostics hook.
///
/// The hook is process-wide and idempotent. It writes panic reports to
/// [`CRASH_DUMP_DIR_ENV`] when set, otherwise to
/// `std::env::temp_dir()/running-process/crash-dumps`.
pub fn install(component: &str) -> Result<(), CrashDumpError> {
    validate_component(component)?;
    let dir = default_crash_dump_dir();
    fs::create_dir_all(&dir).map_err(|source| CrashDumpError::Directory {
        path: dir.clone(),
        source,
    })?;

    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }

    let component = component.to_string();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let sequence = CRASH_SEQUENCE.fetch_add(1, Ordering::AcqRel);
        let timestamp_millis = current_unix_timestamp_millis();
        let path = crash_report_path(
            &dir,
            &component,
            std::process::id(),
            timestamp_millis,
            sequence,
        );
        if let Err(err) = write_panic_report(&path, &component, info) {
            let _ = writeln!(
                std::io::stderr(),
                "failed to write broker crash report to {path:?}: {err}"
            );
        }
        previous_hook(info);
    }));

    Ok(())
}

fn default_crash_dump_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(CRASH_DUMP_DIR_ENV) {
        if !path.as_os_str().is_empty() {
            return PathBuf::from(path);
        }
    }
    std::env::temp_dir()
        .join("running-process")
        .join("crash-dumps")
}

fn validate_component(component: &str) -> Result<(), CrashDumpError> {
    if component_is_valid(component) {
        Ok(())
    } else {
        Err(CrashDumpError::InvalidComponent {
            component: component.to_string(),
        })
    }
}

fn component_is_valid(component: &str) -> bool {
    let bytes = component.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

fn current_unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn crash_report_path(
    dir: &Path,
    component: &str,
    pid: u32,
    timestamp_millis: u128,
    sequence: u64,
) -> PathBuf {
    dir.join(format!(
        "{component}-{pid}-{timestamp_millis}-{sequence}.panic.txt"
    ))
}

fn write_panic_report(
    path: &Path,
    component: &str,
    info: &std::panic::PanicHookInfo<'_>,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");

    writeln!(file, "component: {component}")?;
    writeln!(file, "pid: {}", std::process::id())?;
    writeln!(file, "thread: {thread_name}")?;
    writeln!(
        file,
        "timestamp_millis: {}",
        current_unix_timestamp_millis()
    )?;
    match info.location() {
        Some(location) => {
            writeln!(
                file,
                "location: {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )?;
        }
        None => {
            writeln!(file, "location: <unknown>")?;
        }
    }
    writeln!(file, "payload: {}", panic_payload(info))?;
    writeln!(file)?;
    writeln!(file, "backtrace:")?;
    writeln!(file, "{}", Backtrace::force_capture())?;
    Ok(())
}

fn panic_payload(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_names_are_filename_safe() {
        assert!(component_is_valid("broker"));
        assert!(component_is_valid("broker_v1"));
        assert!(component_is_valid("broker-v1"));
        assert!(!component_is_valid(""));
        assert!(!component_is_valid("../broker"));
        assert!(!component_is_valid("broker v1"));
        assert!(!component_is_valid(&"a".repeat(65)));
    }

    #[test]
    fn crash_report_path_includes_component_pid_timestamp_and_sequence() {
        let path = crash_report_path(Path::new("/tmp/dumps"), "broker", 42, 1234, 7);
        assert_eq!(
            path,
            Path::new("/tmp/dumps").join("broker-42-1234-7.panic.txt")
        );
    }

    #[test]
    fn invalid_component_reports_original_value() {
        let err = validate_component("bad/name").unwrap_err();
        assert!(matches!(
            err,
            CrashDumpError::InvalidComponent { component } if component == "bad/name"
        ));
    }
}
