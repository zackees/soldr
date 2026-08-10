//! In-memory registry of daemon-owned pipe-backed sessions
//! (issue #130 milestone 3).
//!
//! Architecture mirrors [`crate::daemon::pty_sessions`] but for plain
//! stdin/stdout/stderr pipes instead of a PTY. Each session owns a
//! [`NativeProcess`] (child with three OS pipes) plus a bounded ring buffer
//! per output stream and an optional attached-client mpsc sender per
//! stream. Two reader threads drain stdout / stderr into their buffers and
//! forward to attached clients when present. Stdin is write-only and
//! exposed as an RPC (`WritePipeStdinRequest`) rather than a streaming
//! attach.

use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    CommandSpec, NativeProcess, ProcessConfig, ProcessError, ReadStatus, StderrMode, StdinMode,
    StreamKind,
};
use tokio::sync::mpsc;
use tracing::debug;

use crate::daemon::observer_registry::ObserverRegistry;
use crate::daemon::pty_sessions::{
    AttachmentEnded, ExitState, OutboundFrame, PendingTermination, RingBuffer, TerminationOutcome,
};
use crate::daemon::telemetry::{
    TeeEvent, TeeFileOptions, TeeHandle, TeeOptions, TeeRawOptions, TeeRegistry, TeeSnapshot,
    TeeStatus, TeeStream,
};
use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};

pub const DEFAULT_BACKLOG_BYTES: usize = 1_048_576;
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Per-stream state
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PipeStreamSelect {
    Stdout,
    Stderr,
}

impl PipeStreamSelect {
    fn to_stream_kind(self) -> StreamKind {
        match self {
            Self::Stdout => StreamKind::Stdout,
            Self::Stderr => StreamKind::Stderr,
        }
    }

    fn to_tee_stream(self) -> TeeStream {
        match self {
            Self::Stdout => TeeStream::Stdout,
            Self::Stderr => TeeStream::Stderr,
        }
    }
}

struct AttachedStreamClient {
    sender: mpsc::UnboundedSender<OutboundFrame>,
}

struct PipeStreamState {
    backlog: Mutex<RingBuffer>,
    attached: Mutex<Option<AttachedStreamClient>>,
}

impl PipeStreamState {
    fn new() -> Self {
        Self {
            backlog: Mutex::new(RingBuffer::new(DEFAULT_BACKLOG_BYTES)),
            attached: Mutex::new(None),
        }
    }
}

/// Handle returned by [`OwnedPipeSession::attach_stream`]. The streaming
/// server pulls from `receiver` and forwards to the socket as
/// `PipeStreamFrame`.
pub struct PipeAttachmentHandle {
    pub receiver: mpsc::UnboundedReceiver<OutboundFrame>,
}

#[derive(Debug)]
pub enum PipeAttachError {
    AlreadyAttached,
    SessionExited(ExitState),
    StreamUnavailable,
}

// ---------------------------------------------------------------------------
// OwnedPipeSession
// ---------------------------------------------------------------------------

pub struct OwnedPipeSession {
    pub id: String,
    pub process: Arc<NativeProcess>,
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub originator: String,
    pub created_at_unix: f64,
    pub merge_stderr_into_stdout: bool,
    stdout: PipeStreamState,
    stderr: PipeStreamState,
    tees: TeeRegistry,
    /// Per-session registry of observer event sinks (#221 Phase 2 / #429).
    /// Empty by default; populated via `RegisterSessionObserver` IPC.
    pub(crate) observers: ObserverRegistry,
    stdin_closed: AtomicBool,
    exit_state: Mutex<Option<ExitState>>,
    pub(crate) pending_termination: Mutex<Option<PendingTermination>>,
    /// Set by the grace-window timer thread when it fires the hard kill
    /// because the child didn't honor the soft signal in time. Used by
    /// `classify_termination` to distinguish SoftExit (timing-only) from
    /// HardKilled (explicit `.kill()` invocation).
    hard_kill_fired: Arc<AtomicBool>,
    reader_shutdown: Arc<AtomicBool>,
    reader_threads: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl OwnedPipeSession {
    fn stream_state(&self, stream: PipeStreamSelect) -> &PipeStreamState {
        match stream {
            PipeStreamSelect::Stdout => &self.stdout,
            PipeStreamSelect::Stderr => &self.stderr,
        }
    }

    pub fn exit_state(&self) -> Option<ExitState> {
        self.exit_state.lock().unwrap().clone()
    }

    pub fn is_attached(&self, stream: PipeStreamSelect) -> bool {
        self.stream_state(stream).attached.lock().unwrap().is_some()
    }

    /// If true, stdout and stderr were merged at spawn time; attempts to
    /// attach to stderr return [`PipeAttachError::StreamUnavailable`].
    pub fn stream_available(&self, stream: PipeStreamSelect) -> bool {
        match stream {
            PipeStreamSelect::Stdout => true,
            PipeStreamSelect::Stderr => !self.merge_stderr_into_stdout,
        }
    }

    pub fn attach_stream(
        &self,
        stream: PipeStreamSelect,
        steal: bool,
    ) -> Result<(PipeAttachmentHandle, Vec<u8>, u64), PipeAttachError> {
        if !self.stream_available(stream) {
            return Err(PipeAttachError::StreamUnavailable);
        }
        if let Some(s) = self.exit_state() {
            return Err(PipeAttachError::SessionExited(s));
        }
        let state = self.stream_state(stream);
        let mut attached = state.attached.lock().unwrap();
        if attached.is_some() {
            if !steal {
                return Err(PipeAttachError::AlreadyAttached);
            }
            if let Some(existing) = attached.take() {
                let _ = existing
                    .sender
                    .send(OutboundFrame::Ended(AttachmentEnded::Stolen));
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (backlog, dropped) = state.backlog.lock().unwrap().drain();
        *attached = Some(AttachedStreamClient { sender: tx });
        Ok((PipeAttachmentHandle { receiver: rx }, backlog, dropped))
    }

    pub fn clear_attachment(&self, stream: PipeStreamSelect) {
        *self.stream_state(stream).attached.lock().unwrap() = None;
    }

    /// Snapshot the ring-buffer contents for one stream without
    /// consuming them (#130 M7 B4 "sessions log").
    pub fn backlog_snapshot(&self, stream: PipeStreamSelect) -> (Vec<u8>, u64) {
        self.stream_state(stream).backlog.lock().unwrap().snapshot()
    }

    /// Register a non-blocking bounded ring tee for stdout or stderr.
    pub fn tee_stream_ring(
        &self,
        stream: PipeStreamSelect,
        capacity: usize,
    ) -> Result<TeeHandle, PipeAttachError> {
        if !self.stream_available(stream) {
            return Err(PipeAttachError::StreamUnavailable);
        }
        Ok(self.tees.add_ring(stream.to_tee_stream(), capacity))
    }

    /// Register a bounded non-blocking channel tee for stdout or stderr.
    pub fn tee_stream_channel(
        &self,
        stream: PipeStreamSelect,
        capacity: usize,
    ) -> Result<(TeeHandle, Receiver<TeeEvent>), PipeAttachError> {
        self.tee_stream_channel_with_options(stream, capacity, TeeOptions::default())
    }

    /// Register a bounded channel tee for stdout or stderr.
    pub fn tee_stream_channel_with_options(
        &self,
        stream: PipeStreamSelect,
        capacity: usize,
        options: TeeOptions,
    ) -> Result<(TeeHandle, Receiver<TeeEvent>), PipeAttachError> {
        if !self.stream_available(stream) {
            return Err(PipeAttachError::StreamUnavailable);
        }
        Ok(self
            .tees
            .add_channel_with_options(stream.to_tee_stream(), capacity, options))
    }

    /// Register a callback tee for stdout or stderr.
    pub fn tee_stream_callback<F>(
        &self,
        stream: PipeStreamSelect,
        capacity: usize,
        callback: F,
    ) -> Result<TeeHandle, PipeAttachError>
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        self.tee_stream_callback_with_options(stream, capacity, TeeOptions::default(), callback)
    }

    /// Register a callback tee for stdout or stderr.
    pub fn tee_stream_callback_with_options<F>(
        &self,
        stream: PipeStreamSelect,
        capacity: usize,
        options: TeeOptions,
        callback: F,
    ) -> Result<TeeHandle, PipeAttachError>
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        if !self.stream_available(stream) {
            return Err(PipeAttachError::StreamUnavailable);
        }
        Ok(self
            .tees
            .add_callback_with_options(stream.to_tee_stream(), capacity, options, callback))
    }

    /// Register a file path tee for stdout or stderr.
    pub fn tee_stream_file<P>(
        &self,
        stream: PipeStreamSelect,
        path: P,
        options: TeeFileOptions,
    ) -> io::Result<TeeHandle>
    where
        P: AsRef<Path>,
    {
        if !self.stream_available(stream) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe stream unavailable",
            ));
        }
        self.tees.add_file(stream.to_tee_stream(), path, options)
    }

    /// Register a raw file descriptor tee for stdout or stderr.
    #[cfg(unix)]
    pub fn tee_stream_raw_fd(
        &self,
        stream: PipeStreamSelect,
        fd: RawFd,
        options: TeeRawOptions,
    ) -> io::Result<TeeHandle> {
        if !self.stream_available(stream) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe stream unavailable",
            ));
        }
        Ok(self.tees.add_raw_fd(stream.to_tee_stream(), fd, options))
    }

    /// Register a raw Windows handle tee for stdout or stderr.
    #[cfg(windows)]
    pub fn tee_stream_raw_handle(
        &self,
        stream: PipeStreamSelect,
        handle: RawHandle,
        options: TeeRawOptions,
    ) -> io::Result<TeeHandle> {
        if !self.stream_available(stream) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipe stream unavailable",
            ));
        }
        Ok(self
            .tees
            .add_raw_handle(stream.to_tee_stream(), handle, options))
    }

    /// Register a non-blocking bounded ring tee for bytes written to stdin.
    pub fn tee_input_ring(&self, capacity: usize) -> TeeHandle {
        self.tees.add_ring(TeeStream::Stdin, capacity)
    }

    /// Register a bounded non-blocking channel tee for bytes written to stdin.
    pub fn tee_input_channel(&self, capacity: usize) -> (TeeHandle, Receiver<TeeEvent>) {
        self.tee_input_channel_with_options(capacity, TeeOptions::default())
    }

    /// Register a bounded channel tee for bytes written to stdin.
    pub fn tee_input_channel_with_options(
        &self,
        capacity: usize,
        options: TeeOptions,
    ) -> (TeeHandle, Receiver<TeeEvent>) {
        self.tees
            .add_channel_with_options(TeeStream::Stdin, capacity, options)
    }

    /// Register a callback tee for bytes written to stdin.
    pub fn tee_input_callback<F>(&self, capacity: usize, callback: F) -> TeeHandle
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        self.tee_input_callback_with_options(capacity, TeeOptions::default(), callback)
    }

    /// Register a callback tee for bytes written to stdin.
    pub fn tee_input_callback_with_options<F>(
        &self,
        capacity: usize,
        options: TeeOptions,
        callback: F,
    ) -> TeeHandle
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        self.tees
            .add_callback_with_options(TeeStream::Stdin, capacity, options, callback)
    }

    /// Register a file path tee for bytes written to stdin.
    pub fn tee_input_file<P>(&self, path: P, options: TeeFileOptions) -> io::Result<TeeHandle>
    where
        P: AsRef<Path>,
    {
        self.tees.add_file(TeeStream::Stdin, path, options)
    }

    /// Register a raw file descriptor tee for bytes written to stdin.
    #[cfg(unix)]
    pub fn tee_input_raw_fd(&self, fd: RawFd, options: TeeRawOptions) -> TeeHandle {
        self.tees.add_raw_fd(TeeStream::Stdin, fd, options)
    }

    /// Register a raw Windows handle tee for bytes written to stdin.
    #[cfg(windows)]
    pub fn tee_input_raw_handle(&self, handle: RawHandle, options: TeeRawOptions) -> TeeHandle {
        self.tees.add_raw_handle(TeeStream::Stdin, handle, options)
    }

    /// Snapshot a ring tee without draining it.
    pub fn tee_snapshot(&self, handle: TeeHandle) -> Option<TeeSnapshot> {
        self.tees.snapshot(handle)
    }

    /// Return current missed-byte status for any tee sink.
    pub fn tee_status(&self, handle: TeeHandle) -> Option<TeeStatus> {
        self.tees.status(handle)
    }

    /// Remove a registered tee sink.
    pub fn untee(&self, handle: TeeHandle) -> bool {
        self.tees.remove(handle)
    }

    pub fn notify_attached(&self, stream: PipeStreamSelect, frame: OutboundFrame) {
        if let Some(client) = self.stream_state(stream).attached.lock().unwrap().as_ref() {
            let _ = client.sender.send(frame);
        }
    }

    pub fn write_stdin(&self, bytes: &[u8], close_after: bool) -> Result<usize, ProcessError> {
        if self.stdin_closed.load(Ordering::Acquire) {
            return Err(ProcessError::StdinUnavailable);
        }
        if !bytes.is_empty() {
            self.process.write_stdin_streaming(bytes)?;
            self.tees.write(TeeStream::Stdin, bytes);
        }
        if close_after {
            self.process.close_stdin()?;
            self.stdin_closed.store(true, Ordering::Release);
        }
        Ok(bytes.len())
    }

    /// Soft-then-hard termination. M4 will replace this with the
    /// configurable schedule that records the exit path; for M3 the
    /// grace window is observed but the soft signal is just `kill()`
    /// (NativeProcess does not expose a soft-signal API as of this
    /// commit; the hard kill arrives within the grace window anyway).
    pub fn terminate(&self, grace: Duration) -> Result<(), ProcessError> {
        if self.process.poll()?.is_some() {
            return Ok(());
        }
        *self.pending_termination.lock().unwrap() = Some(PendingTermination {
            started_at_unix: unix_now(),
            grace_secs: grace.as_secs_f64(),
        });
        // Soft step: SIGTERM to the child's process group on POSIX,
        // no-op on Windows (until CTRL_BREAK_EVENT plumbing lands as a
        // separate follow-up).
        let _ = self.process.terminate_group_soft();
        let process = Arc::clone(&self.process);
        let hard_kill_fired = Arc::clone(&self.hard_kill_fired);
        thread::spawn(move || {
            // #199: intentional — the grace period IS the wait. After
            // SIGTERM we give the child a fixed window to exit
            // cleanly before escalating to SIGKILL. The signaling
            // alternative (waitpid + alarm) doesn't compose well
            // with the existing tokio-based daemon runtime.
            thread::sleep(grace);
            if process.poll().ok().flatten().is_none() {
                hard_kill_fired.store(true, Ordering::Release);
                let _ = process.kill();
            }
        });
        Ok(())
    }

    pub(crate) fn classify_termination(&self, exited_at_unix: f64) -> TerminationOutcome {
        match *self.pending_termination.lock().unwrap() {
            None => TerminationOutcome::NaturalExit,
            Some(p) => {
                if self.hard_kill_fired.load(Ordering::Acquire) {
                    TerminationOutcome::HardKilled
                } else if exited_at_unix - p.started_at_unix <= p.grace_secs + 0.25 {
                    TerminationOutcome::SoftExit
                } else {
                    TerminationOutcome::HardKilled
                }
            }
        }
    }

    /// Mark the session for reader shutdown. Subsequent reader iterations
    /// will see this and exit. Public for daemon shutdown path.
    pub fn signal_shutdown(&self) {
        self.reader_shutdown.store(true, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct PipeSessionRegistry {
    sessions: Mutex<HashMap<String, Arc<OwnedPipeSession>>>,
    next_id: AtomicU64,
}

impl PipeSessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    pub fn remove(&self, id: &str) -> Option<Arc<OwnedPipeSession>> {
        self.sessions.lock().unwrap().remove(id)
    }

    /// Remove every session in the registry that has already exited.
    /// Returns the number of removed entries. Optionally filtered by
    /// originator (empty matches all).
    pub fn purge_exited(&self, originator: &str) -> usize {
        let mut guard = self.sessions.lock().unwrap();
        let to_remove: Vec<String> = guard
            .iter()
            .filter(|(_, s)| {
                s.exit_state().is_some() && (originator.is_empty() || s.originator == originator)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in &to_remove {
            guard.remove(k);
        }
        to_remove.len()
    }

    /// Spawn a new pipe-backed child and register it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        self: &Arc<Self>,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        originator: String,
        command_display: String,
        merge_stderr_into_stdout: bool,
    ) -> Result<Arc<OwnedPipeSession>, SpawnError> {
        if argv.is_empty() {
            return Err(SpawnError::EmptyArgv);
        }

        let config = ProcessConfig {
            command: CommandSpec::Argv(argv.clone()),
            cwd: cwd.clone().map(std::path::PathBuf::from),
            env,
            capture: true,
            stderr_mode: if merge_stderr_into_stdout {
                StderrMode::Stdout
            } else {
                StderrMode::Pipe
            },
            creationflags: None,
            // Put each pipe-backed child in its own process group so
            // both the POSIX SIGTERM path (kill(-pgid, SIGTERM)) and
            // the Windows CTRL_BREAK_EVENT path
            // (GenerateConsoleCtrlEvent with CREATE_NEW_PROCESS_GROUP)
            // route to the child's own tree and not the daemon's.
            create_process_group: true,
            stdin_mode: StdinMode::Piped,
            nice: None,
        };
        let process = NativeProcess::new(config);
        process
            .start()
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;

        let pid = process.pid().unwrap_or(0);
        let id = self.next_session_id();

        let session = Arc::new(OwnedPipeSession {
            id: id.clone(),
            process: Arc::new(process),
            pid,
            command: command_display,
            cwd: cwd.unwrap_or_default(),
            originator,
            created_at_unix: unix_now(),
            merge_stderr_into_stdout,
            stdout: PipeStreamState::new(),
            stderr: PipeStreamState::new(),
            tees: TeeRegistry::new(),
            observers: ObserverRegistry::new(),
            stdin_closed: AtomicBool::new(false),
            exit_state: Mutex::new(None),
            pending_termination: Mutex::new(None),
            hard_kill_fired: Arc::new(AtomicBool::new(false)),
            reader_shutdown: Arc::new(AtomicBool::new(false)),
            reader_threads: Mutex::new(Vec::new()),
        });

        // Spawn reader threads for stdout and stderr.
        let mut handles = Vec::new();
        handles.push(thread::spawn({
            let session = Arc::clone(&session);
            move || reader_loop(session, PipeStreamSelect::Stdout)
        }));
        if !merge_stderr_into_stdout {
            handles.push(thread::spawn({
                let session = Arc::clone(&session);
                move || reader_loop(session, PipeStreamSelect::Stderr)
            }));
        }
        // Spawn exit waiter that sets exit_state once the child exits.
        handles.push(thread::spawn({
            let session = Arc::clone(&session);
            move || exit_waiter_loop(session)
        }));
        *session.reader_threads.lock().unwrap() = handles;

        // Phase 2 lifecycle hook: emit Started to any observer sink that
        // registers on this session. Same caveat as PTY sessions —
        // observers added later do not see pre-registration events.
        session.observers.emit(ObserverEvent::new_now(
            EventCategory::Lifecycle,
            ObserverEventKind::Started,
            session.pid,
        ));

        self.sessions
            .lock()
            .unwrap()
            .insert(id, Arc::clone(&session));
        Ok(session)
    }

    fn next_session_id(&self) -> String {
        let counter = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("pipe-{nanos:016x}-{counter:08x}")
    }
}

impl Default for PipeSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SpawnError {
    EmptyArgv,
    Spawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyArgv => write!(f, "argv must not be empty"),
            SpawnError::Spawn(s) => write!(f, "failed to spawn pipe session: {s}"),
        }
    }
}

impl std::error::Error for SpawnError {}

// ---------------------------------------------------------------------------
// Reader threads
// ---------------------------------------------------------------------------

fn reader_loop(session: Arc<OwnedPipeSession>, stream: PipeStreamSelect) {
    let stream_kind = stream.to_stream_kind();
    loop {
        if session.reader_shutdown.load(Ordering::Acquire) {
            break;
        }
        match session
            .process
            .read_stream(stream_kind, Some(Duration::from_millis(100)))
        {
            ReadStatus::Line(bytes) => {
                // NativeProcess::read_stream returns one line at a
                // time with the trailing newline already stripped. Add
                // a single '\n' back so the backlog preserves line
                // structure - downstream consumers expect to see the
                // bytes as the child wrote them.
                let state = session.stream_state(stream);
                let mut with_lf = bytes;
                with_lf.push(b'\n');
                state.backlog.lock().unwrap().push(&with_lf);
                session.tees.write(stream.to_tee_stream(), &with_lf);
                if let Some(client) = state.attached.lock().unwrap().as_ref() {
                    for slice in with_lf.chunks(STREAM_CHUNK_BYTES) {
                        let _ = client.sender.send(OutboundFrame::Output(slice.to_vec()));
                    }
                }
            }
            ReadStatus::Timeout => {
                // No data within the window; loop.
            }
            ReadStatus::Eof => {
                debug!(
                    session_id = %session.id,
                    stream = stream_kind.as_str(),
                    "pipe stream reached EOF"
                );
                // Notify the attached client (if any) and stop reading.
                session.notify_attached(stream, OutboundFrame::Ended(AttachmentEnded::Detached));
                break;
            }
        }
    }
}

fn exit_waiter_loop(session: Arc<OwnedPipeSession>) {
    // Block until exit, then record final state.
    let exit_code = match session.process.wait(None) {
        Ok(code) => code,
        Err(_) => {
            // wait returned an error (NotRunning or similar). Treat as
            // unrecoverable but do not fabricate a code.
            return;
        }
    };
    let exited_at_unix = unix_now();
    let outcome = session.classify_termination(exited_at_unix);
    let state = ExitState {
        exit_code,
        exited_at_unix,
        outcome,
    };
    *session.exit_state.lock().unwrap() = Some(state.clone());
    // Phase 2 lifecycle hook: emit Exited to any registered observer sinks.
    session.observers.emit(ObserverEvent::new_now(
        EventCategory::Lifecycle,
        ObserverEventKind::Exited {
            exit_code: state.exit_code,
        },
        session.pid,
    ));
    // Notify any attached stream clients.
    for stream in [PipeStreamSelect::Stdout, PipeStreamSelect::Stderr] {
        if let Some(client) = session.stream_state(stream).attached.lock().unwrap().take() {
            let _ = client.sender.send(OutboundFrame::Exit(state.exit_code));
            let _ = client
                .sender
                .send(OutboundFrame::Ended(AttachmentEnded::SessionExited));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_assigns_unique_pipe_ids() {
        let r = Arc::new(PipeSessionRegistry::new());
        let a = r.next_session_id();
        let b = r.next_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("pipe-"));
    }
}
