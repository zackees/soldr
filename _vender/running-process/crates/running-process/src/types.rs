use std::path::PathBuf;

use thiserror::Error;

/// Output stream selector used by process read and capture APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl StreamKind {
    /// Return the stable lowercase stream name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// One captured line or chunk tagged with its source stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    /// Stream that produced `line`.
    pub stream: StreamKind,
    /// Raw bytes read from the stream.
    pub line: Vec<u8>,
}

/// Result of a bounded process read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadStatus<T> {
    /// A line or chunk was read.
    Line(T),
    /// The read deadline elapsed before data arrived.
    Timeout,
    /// The stream reached end-of-file.
    Eof,
}

/// Error returned by process lifecycle and I/O operations.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// Start was requested for a process that has already been started.
    #[error("process already started")]
    AlreadyStarted,
    /// The operation requires a running child process.
    #[error("process is not running")]
    NotRunning,
    /// The process was not configured with piped stdin.
    #[error("process stdin is not available")]
    StdinUnavailable,
    /// Child process creation failed.
    #[error("failed to spawn process: {0}")]
    Spawn(std::io::Error),
    /// Reading or writing child process streams failed.
    #[error("failed to read process output: {0}")]
    Io(std::io::Error),
    /// The requested wait or read operation timed out.
    #[error("process timed out")]
    Timeout,
    /// Captured stdout and stderr exceeded the caller's aggregate byte limit.
    #[error("captured process output exceeded the {limit}-byte limit")]
    OutputLimitExceeded {
        /// Aggregate stdout/stderr capture limit.
        limit: usize,
    },
}

/// Captured output and exit status returned by one-shot process helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// Raw stdout bytes captured from the child.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes captured from the child.
    pub stderr: Vec<u8>,
    /// Process exit code, with Unix signal exits represented as negative signal numbers.
    pub exit_code: i32,
}

/// Command representation used by [`ProcessConfig`].
#[derive(Debug, Clone)]
pub enum CommandSpec {
    /// Execute a command line through the platform shell.
    Shell(String),
    /// Execute a program and argument vector directly.
    Argv(Vec<String>),
}

/// Stdin behavior for a spawned process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    /// Inherit stdin from the current process.
    Inherit,
    /// Create a pipe so callers can write to child stdin.
    Piped,
    /// Connect child stdin to the platform null device.
    Null,
}

/// Stderr handling for a spawned process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrMode {
    /// Merge stderr into stdout handling.
    Stdout,
    /// Capture stderr through its own pipe.
    Pipe,
}

/// Configuration for [`crate::NativeProcess`].
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Command line or argv to execute.
    pub command: CommandSpec,
    /// Working directory for the child process.
    pub cwd: Option<PathBuf>,
    /// Environment overrides passed to the child process.
    pub env: Option<Vec<(String, String)>>,
    /// Whether stdout/stderr should be retained in capture history.
    pub capture: bool,
    /// How stderr should be routed.
    pub stderr_mode: StderrMode,
    /// Windows process creation flags.
    pub creationflags: Option<u32>,
    /// Whether to create a new process group where supported.
    pub create_process_group: bool,
    /// How stdin should be routed.
    pub stdin_mode: StdinMode,
    /// Nice value to apply on Unix-like platforms.
    pub nice: Option<i32>,
}
