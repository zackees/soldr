//! In-memory registry of daemon-owned PTY sessions (issue #130 milestone 2).
//!
//! Each session owns a [`NativePtyProcess`] (PTY master + child process) plus
//! a bounded ring buffer of recent output, an optional attached-client mpsc
//! sender, and an exit-state slot. A background reader thread drains output
//! from the PTY into the ring buffer and into the attached client when one is
//! present. Detach drops the mpsc sender but leaves the reader running — the
//! ring buffer keeps filling so a later attach sees a backlog.
//!
//! Sessions live only in process memory. Daemon restart loses them; M8
//! adoption is intentionally out of scope here (the PTY master handle cannot
//! be re-acquired by a new daemon process without a kernel-side broker).

use std::collections::{HashMap, VecDeque};
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawHandle;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::pty::NativePtyProcess;
use crate::terminal_graphics::TerminalGraphicsCapabilities;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::daemon::observer_registry::ObserverRegistry;
use crate::daemon::telemetry::{
    TeeEvent, TeeFileOptions, TeeHandle, TeeOptions, TeeRawOptions, TeeRegistry, TeeSnapshot,
    TeeStatus, TeeStream,
};
use crate::observer::{EventCategory, ObserverEvent, ObserverEventKind};

/// Default ring-buffer capacity for the output backlog (1 MiB per session).
pub const DEFAULT_BACKLOG_BYTES: usize = 1_048_576;

/// Maximum payload size of a single `PtyStreamFrame.output` chunk sent to an
/// attached client. Larger reads from the PTY are split.
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------

/// Bounded byte buffer that drops the oldest bytes when capacity is exceeded.
/// Tracks the cumulative count of dropped bytes so attaching clients can be
/// told "you missed N bytes before this backlog starts."
pub struct RingBuffer {
    capacity: usize,
    data: VecDeque<u8>,
    bytes_dropped: u64,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            bytes_dropped: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        // If the incoming slice is larger than the whole buffer, only keep
        // the tail.
        let (slice, extra_dropped) = if bytes.len() > self.capacity {
            let drop_n = bytes.len() - self.capacity;
            (&bytes[drop_n..], drop_n as u64)
        } else {
            (bytes, 0)
        };

        // Drop existing bytes from the head to make room.
        let overflow = (self.data.len() + slice.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.data.drain(..overflow);
            self.bytes_dropped += overflow as u64;
        }
        self.bytes_dropped += extra_dropped;
        self.data.extend(slice);
    }

    /// Drain the current contents into a `Vec<u8>`, returning the bytes and
    /// the cumulative dropped-byte counter at the moment of the drain.
    pub fn drain(&mut self) -> (Vec<u8>, u64) {
        let bytes: Vec<u8> = self.data.drain(..).collect();
        (bytes, self.bytes_dropped)
    }

    /// Copy the current contents WITHOUT draining the buffer. Used by
    /// `GetSessionBacklog` (#130 M7 B4) so callers can snapshot the
    /// captured output without disturbing the buffer that a future
    /// attach would replay.
    pub fn snapshot(&self) -> (Vec<u8>, u64) {
        (self.data.iter().copied().collect(), self.bytes_dropped)
    }

    pub fn dropped(&self) -> u64 {
        self.bytes_dropped
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Attached client + exit state
// ---------------------------------------------------------------------------

/// Reason an attached client was evicted.
#[derive(Debug, Clone)]
pub enum AttachmentEnded {
    /// The session exited; final exit code is in `ExitState`.
    SessionExited,
    /// A peer attached with `steal=true` and replaced this client.
    Stolen,
    /// The session was terminated by an explicit request.
    Terminated,
    /// The client requested detach.
    Detached,
}

/// Frames sent from the reader thread / handlers to the currently attached
/// client. Plain bytes here; the streaming server encodes them as
/// `PtyStreamFrame` protobuf before writing to the socket.
#[derive(Debug, Clone)]
pub enum OutboundFrame {
    Output(Vec<u8>),
    MissedBytes(u64),
    Exit(i32),
    Ended(AttachmentEnded),
}

/// One end of the duplex stream; held by the streaming server task.
pub struct AttachmentHandle {
    pub receiver: mpsc::UnboundedReceiver<OutboundFrame>,
}

/// State held inside the session for the currently attached client.
struct AttachedClient {
    sender: mpsc::UnboundedSender<OutboundFrame>,
    /// Whether the attaching client identified itself as a real TTY.
    /// `false` clients are in the C9 "degraded" mode: the daemon will
    /// skip side effects that only make sense for an interactive
    /// terminal (e.g. resize). Bytes still flow normally.
    is_tty: bool,
    /// Client-supplied TERM value. Recorded for the session's lifetime
    /// so list/snapshot can surface it.
    term: String,
    /// Client-supplied terminal graphics capability metadata. Missing
    /// metadata from older clients is represented as unknown.
    graphics_capabilities: TerminalGraphicsCapabilities,
}

/// Final state once the child exits.
#[derive(Debug, Clone)]
pub struct ExitState {
    pub exit_code: i32,
    pub exited_at_unix: f64,
    pub outcome: TerminationOutcome,
}

/// Which termination path a session took (mirrors the proto enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationOutcome {
    Unspecified,
    NaturalExit,
    SoftExit,
    HardKilled,
}

/// Signal state recorded by [`OwnedPtySession::terminate`] so the
/// exit-waiter / reader thread can classify the eventual exit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingTermination {
    pub started_at_unix: f64,
    pub grace_secs: f64,
}

// ---------------------------------------------------------------------------
// OwnedPtySession
// ---------------------------------------------------------------------------

/// One daemon-owned PTY session. Held in an `Arc` inside `PtySessionRegistry`.
pub struct OwnedPtySession {
    pub id: String,
    pub process: Arc<NativePtyProcess>,
    pub pid: u32,
    pub command: String,
    pub cwd: String,
    pub originator: String,
    pub created_at_unix: f64,
    pub rows: AtomicU16,
    pub cols: AtomicU16,
    backlog: Mutex<RingBuffer>,
    tees: TeeRegistry,
    /// Per-session registry of observer event sinks (#221 Phase 2 / #429).
    /// Empty by default; populated via `RegisterSessionObserver` IPC.
    pub(crate) observers: ObserverRegistry,
    attached: Mutex<Option<AttachedClient>>,
    exit_state: Mutex<Option<ExitState>>,
    pub(crate) pending_termination: Mutex<Option<PendingTermination>>,
    /// Set by the grace-window timer thread when it fires the hard kill
    /// because the child didn't honor the soft signal in time. Used by
    /// `classify_termination` to distinguish SoftExit (timing-only) from
    /// HardKilled (explicit `.kill_tree_impl()` invocation).
    hard_kill_fired: Arc<AtomicBool>,
    reader_shutdown: Arc<AtomicBool>,
    reader_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl OwnedPtySession {
    pub fn is_attached(&self) -> bool {
        self.attached.lock().unwrap().is_some()
    }

    pub fn exit_state(&self) -> Option<ExitState> {
        self.exit_state.lock().unwrap().clone()
    }

    pub fn rows(&self) -> u16 {
        self.rows.load(Ordering::Acquire)
    }

    pub fn cols(&self) -> u16 {
        self.cols.load(Ordering::Acquire)
    }

    /// Snapshot the current ring-buffer contents without consuming them
    /// (#130 M7 B4 "sessions log").
    pub fn backlog_snapshot(&self) -> (Vec<u8>, u64) {
        self.backlog.lock().unwrap().snapshot()
    }

    /// Register a non-blocking bounded ring tee for PTY output bytes.
    pub fn tee_output_ring(&self, capacity: usize) -> TeeHandle {
        self.tees.add_ring(TeeStream::PtyOutput, capacity)
    }

    /// Register a bounded non-blocking channel tee for PTY output bytes.
    pub fn tee_output_channel(&self, capacity: usize) -> (TeeHandle, Receiver<TeeEvent>) {
        self.tee_output_channel_with_options(capacity, TeeOptions::default())
    }

    /// Register a bounded channel tee for PTY output bytes.
    pub fn tee_output_channel_with_options(
        &self,
        capacity: usize,
        options: TeeOptions,
    ) -> (TeeHandle, Receiver<TeeEvent>) {
        self.tees
            .add_channel_with_options(TeeStream::PtyOutput, capacity, options)
    }

    /// Register a callback tee for PTY output bytes.
    pub fn tee_output_callback<F>(&self, capacity: usize, callback: F) -> TeeHandle
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        self.tee_output_callback_with_options(capacity, TeeOptions::default(), callback)
    }

    /// Register a callback tee for PTY output bytes.
    pub fn tee_output_callback_with_options<F>(
        &self,
        capacity: usize,
        options: TeeOptions,
        callback: F,
    ) -> TeeHandle
    where
        F: FnMut(TeeEvent) + Send + 'static,
    {
        self.tees
            .add_callback_with_options(TeeStream::PtyOutput, capacity, options, callback)
    }

    /// Register a file path tee for PTY output bytes.
    pub fn tee_output_file<P>(&self, path: P, options: TeeFileOptions) -> io::Result<TeeHandle>
    where
        P: AsRef<Path>,
    {
        self.tees.add_file(TeeStream::PtyOutput, path, options)
    }

    /// Register a raw file descriptor tee for PTY output bytes.
    #[cfg(unix)]
    pub fn tee_output_raw_fd(&self, fd: RawFd, options: TeeRawOptions) -> TeeHandle {
        self.tees.add_raw_fd(TeeStream::PtyOutput, fd, options)
    }

    /// Register a raw Windows handle tee for PTY output bytes.
    #[cfg(windows)]
    pub fn tee_output_raw_handle(&self, handle: RawHandle, options: TeeRawOptions) -> TeeHandle {
        self.tees
            .add_raw_handle(TeeStream::PtyOutput, handle, options)
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

    /// Whether the currently attached client (if any) self-identified as
    /// a TTY at attach time. Returns `false` when no client is attached.
    pub fn attached_is_tty(&self) -> bool {
        self.attached
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.is_tty)
            .unwrap_or(false)
    }

    /// TERM value supplied by the currently attached client. Empty when
    /// no client is attached.
    pub fn attached_term(&self) -> String {
        self.attached
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.term.clone())
            .unwrap_or_default()
    }

    /// Graphics capability metadata supplied by the currently attached
    /// client. Missing metadata from older clients is represented as
    /// unknown rather than inferred from daemon environment.
    pub fn attached_graphics_capabilities(&self) -> TerminalGraphicsCapabilities {
        self.attached
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.graphics_capabilities.clone())
            .unwrap_or_else(TerminalGraphicsCapabilities::unknown)
    }

    /// Install an attached client. Returns the receiver half + a snapshot of
    /// the current backlog (with the cumulative bytes-dropped counter).
    ///
    /// `steal=false`: returns `Err(AttachError::AlreadyAttached)` if a client
    /// is currently attached.
    /// `steal=true`: evicts the existing attachment by sending
    /// `OutboundFrame::Ended(AttachmentEnded::Stolen)` before installing the
    /// new one.
    pub fn attach(
        &self,
        steal: bool,
        rows: u16,
        cols: u16,
    ) -> Result<(AttachmentHandle, Vec<u8>, u64), AttachError> {
        self.attach_with_terminal_info(
            steal,
            rows,
            cols,
            true,
            String::new(),
            TerminalGraphicsCapabilities::unknown(),
        )
    }

    /// Like [`Self::attach`] but lets the caller record whether the
    /// client is a real TTY and what TERM it claims. Non-TTY clients
    /// skip the resize side effect because pixel dimensions are
    /// meaningless without an interactive terminal (#130 M6 C9).
    pub fn attach_with_terminal_info(
        &self,
        steal: bool,
        rows: u16,
        cols: u16,
        is_tty: bool,
        term: String,
        graphics_capabilities: TerminalGraphicsCapabilities,
    ) -> Result<(AttachmentHandle, Vec<u8>, u64), AttachError> {
        // If the session has already exited, surface that immediately rather
        // than handing out an attachment for a corpse.
        if let Some(state) = self.exit_state() {
            return Err(AttachError::SessionExited(state));
        }

        let mut attached = self.attached.lock().unwrap();
        if attached.is_some() {
            if !steal {
                return Err(AttachError::AlreadyAttached);
            }
            // Evict the existing client.
            if let Some(existing) = attached.take() {
                let _ = existing
                    .sender
                    .send(OutboundFrame::Ended(AttachmentEnded::Stolen));
            }
        }

        // Apply resize before draining the backlog so the client sees
        // output sized for its terminal. Non-TTY clients (stream-JSON
        // renderers etc.) skip this: their rows/cols are meaningless.
        if is_tty {
            self.rows.store(rows, Ordering::Release);
            self.cols.store(cols, Ordering::Release);
            if rows > 0 && cols > 0 {
                if let Err(e) = self.process.resize_impl(rows, cols) {
                    warn!(session_id = %self.id, error = %e, "resize on attach failed");
                }
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let (backlog, dropped) = self.backlog.lock().unwrap().drain();
        *attached = Some(AttachedClient {
            sender: tx,
            is_tty,
            term,
            graphics_capabilities,
        });
        Ok((AttachmentHandle { receiver: rx }, backlog, dropped))
    }

    /// Drop the attached client without notifying it (caller is doing the
    /// notify, e.g. via OutboundFrame::Ended).
    pub fn clear_attachment(&self) {
        *self.attached.lock().unwrap() = None;
    }

    /// Forward a frame to the attached client, if any. Used by the streaming
    /// server to deliver final-state frames.
    pub fn notify_attached(&self, frame: OutboundFrame) {
        if let Some(client) = self.attached.lock().unwrap().as_ref() {
            let _ = client.sender.send(frame);
        }
    }

    /// Write bytes to the PTY input.
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), crate::pty::PtyError> {
        self.process.write_impl(bytes, false)?;
        self.tees.write(TeeStream::Stdin, bytes);
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), crate::pty::PtyError> {
        self.rows.store(rows, Ordering::Release);
        self.cols.store(cols, Ordering::Release);
        self.process.resize_impl(rows, cols)
    }

    pub fn send_interrupt(&self) -> Result<(), crate::pty::PtyError> {
        self.process.send_interrupt_impl()
    }

    /// Begin a graceful-then-hard termination sequence. For M2 this is an
    /// immediate `terminate_tree` followed by a short grace, then `kill_tree`.
    /// M4 will replace this with a configurable schedule running on a tokio
    /// task; the API stays the same.
    pub fn terminate(&self, grace: Duration) -> Result<(), crate::pty::PtyError> {
        // Record the soft-signal moment so the reader loop can classify
        // the eventual exit as Soft (within grace) or HardKilled (after
        // grace window).
        *self.pending_termination.lock().unwrap() = Some(PendingTermination {
            started_at_unix: unix_now(),
            grace_secs: grace.as_secs_f64(),
        });

        self.process.terminate_tree_impl()?;
        let process = Arc::clone(&self.process);
        let hard_kill_fired = Arc::clone(&self.hard_kill_fired);
        thread::spawn(move || {
            // #199: intentional — grace-before-hard-kill mirror of
            // pipe_sessions.rs. The PTY-tree variant uses
            // terminate_tree_impl + kill_tree_impl but the timing
            // semantics are identical.
            thread::sleep(grace);
            if process.wait_impl(Some(0.0)).is_err() {
                hard_kill_fired.store(true, Ordering::Release);
                let _ = process.kill_tree_impl();
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
}

#[derive(Debug)]
pub enum AttachError {
    AlreadyAttached,
    SessionExited(ExitState),
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Map of session id → owned session. Wrapped in `Arc` and shared via
/// `DaemonState`.
pub struct PtySessionRegistry {
    sessions: Mutex<HashMap<String, Arc<OwnedPtySession>>>,
    next_id: AtomicU64,
}

impl PtySessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<OwnedPtySession>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<OwnedPtySession>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// Spawn a new PTY child, register it, and return the session.
    ///
    /// `command_display` is the string written into the session record (for
    /// `ListPtySessions`). It is built by the caller; this module does not
    /// shell-quote.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        self: &Arc<Self>,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        rows: u16,
        cols: u16,
        originator: String,
        command_display: String,
    ) -> Result<Arc<OwnedPtySession>, SpawnError> {
        if argv.is_empty() {
            return Err(SpawnError::EmptyArgv);
        }

        let process = NativePtyProcess::new(argv, cwd.clone(), env, rows, cols, None)
            .map_err(|e| SpawnError::Construct(e.to_string()))?;
        process
            .start_impl()
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;

        let pid = pid_of(&process).unwrap_or(0);
        let id = self.next_session_id();

        let session = Arc::new(OwnedPtySession {
            id: id.clone(),
            process: Arc::new(process),
            pid,
            command: command_display,
            cwd: cwd.unwrap_or_default(),
            originator,
            created_at_unix: unix_now(),
            rows: AtomicU16::new(rows),
            cols: AtomicU16::new(cols),
            backlog: Mutex::new(RingBuffer::new(DEFAULT_BACKLOG_BYTES)),
            tees: TeeRegistry::new(),
            observers: ObserverRegistry::new(),
            attached: Mutex::new(None),
            exit_state: Mutex::new(None),
            pending_termination: Mutex::new(None),
            hard_kill_fired: Arc::new(AtomicBool::new(false)),
            reader_shutdown: Arc::new(AtomicBool::new(false)),
            reader_thread: Mutex::new(None),
        });

        // Spawn the reader thread that drains the PTY into the ring buffer.
        let reader_session = Arc::clone(&session);
        let handle = thread::spawn(move || reader_loop(reader_session));
        *session.reader_thread.lock().unwrap() = Some(handle);

        // Phase 2 lifecycle hook: emit Started to any observer sink that
        // registers on this session. Observers added later still receive
        // future events; events that arrive before the first registration
        // are NOT replayed (per the proto-level documentation).
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

    /// Remove a session from the registry. The caller is responsible for
    /// terminating the child first if desired. Returns the removed session
    /// so the caller can drop it (which stops the reader thread).
    pub fn remove(&self, id: &str) -> Option<Arc<OwnedPtySession>> {
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

    fn next_session_id(&self) -> String {
        // pty-<daemon-start-nanos>-<counter>. Uniqueness is per daemon
        // lifetime; that is enough because pty sessions do not survive
        // daemon restart.
        let counter = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("pty-{nanos:016x}-{counter:08x}")
    }
}

impl Default for PtySessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum SpawnError {
    EmptyArgv,
    Construct(String),
    Spawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyArgv => write!(f, "argv must not be empty"),
            SpawnError::Construct(s) => write!(f, "failed to build PTY process: {s}"),
            SpawnError::Spawn(s) => write!(f, "failed to spawn PTY: {s}"),
        }
    }
}

impl std::error::Error for SpawnError {}

// ---------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------

/// Drain PTY output into the session's ring buffer + attached client.
///
/// Runs on a dedicated OS thread because `NativePtyProcess::read_chunk_impl`
/// is blocking. Exits when the PTY closes AND the child has actually
/// exited; only then is `exit_state` recorded.
fn reader_loop(session: Arc<OwnedPtySession>) {
    let read_timeout = Some(0.1_f64);
    loop {
        if session.reader_shutdown.load(Ordering::Acquire) {
            break;
        }
        match session.process.read_chunk_impl(read_timeout) {
            Ok(Some(bytes)) if !bytes.is_empty() => {
                session.backlog.lock().unwrap().push(&bytes);
                session.tees.write(TeeStream::PtyOutput, &bytes);
                if let Some(client) = session.attached.lock().unwrap().as_ref() {
                    for slice in bytes.chunks(STREAM_CHUNK_BYTES) {
                        let _ = client.sender.send(OutboundFrame::Output(slice.to_vec()));
                    }
                }
            }
            Ok(_) => {
                // Timeout: PTY had no data within the read window. Loop
                // and try again unless shutdown was requested.
            }
            Err(_e) => {
                // PTY reader stream is closed. This can mean either the
                // child has exited (the common case) OR the master's read
                // side errored while the child is still alive (transient
                // ConPTY weirdness, fork glitches, etc.). Verify which by
                // probing `wait_impl`: if it returns OK, the child is
                // truly done and we record final state; otherwise leave
                // exit_state as None and just stop reading.
                debug!(session_id = %session.id, "PTY reader closed; probing child status");
                break;
            }
        }
    }

    // Determine final state. Generous wait window because a child that
    // received SIGTERM may take ~1s to fully exit; this thread is dedicated
    // so blocking is fine.
    match session.process.wait_impl(Some(5.0)) {
        Ok(exit_code) => {
            let exited_at_unix = unix_now();
            let outcome = session.classify_termination(exited_at_unix);
            let state = ExitState {
                exit_code,
                exited_at_unix,
                outcome,
            };
            *session.exit_state.lock().unwrap() = Some(state.clone());
            // Phase 2 lifecycle hook: emit Exited to any registered
            // observer sinks.
            session.observers.emit(ObserverEvent::new_now(
                EventCategory::Lifecycle,
                ObserverEventKind::Exited {
                    exit_code: state.exit_code,
                },
                session.pid,
            ));
            if let Some(client) = session.attached.lock().unwrap().take() {
                let _ = client.sender.send(OutboundFrame::Exit(state.exit_code));
                let _ = client
                    .sender
                    .send(OutboundFrame::Ended(AttachmentEnded::SessionExited));
            }
        }
        Err(_) => {
            // PTY stream closed but the child is still alive somehow.
            // We can no longer surface output, but do NOT mark the session
            // as exited — that would lie to `ListPtySessions`. The session
            // will be reaped when the daemon shuts down or on explicit
            // terminate. Drop any attached client so the streaming server
            // sees the channel close.
            debug!(
                session_id = %session.id,
                "PTY reader closed but child still alive; leaving exit_state=None"
            );
            *session.attached.lock().unwrap() = None;
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

fn pid_of(process: &NativePtyProcess) -> Option<u32> {
    // #150: `NativePtyHandles.child` now exposes `PtyChild::pid()`
    // (returning `u32`) instead of portable_pty's `process_id() ->
    // Option<u32>`. The pid is unconditional once handles exist.
    let guard = process.handles.lock().unwrap();
    guard.as_ref().map(|h| h.child.pid())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_drops_oldest_when_capacity_exceeded() {
        let mut rb = RingBuffer::new(8);
        rb.push(b"abcdefgh");
        assert_eq!(rb.len(), 8);
        assert_eq!(rb.dropped(), 0);
        rb.push(b"ij");
        assert_eq!(rb.len(), 8);
        assert_eq!(rb.dropped(), 2);
        let (bytes, dropped) = rb.drain();
        assert_eq!(bytes, b"cdefghij");
        assert_eq!(dropped, 2);
        assert!(rb.is_empty());
    }

    #[test]
    fn ring_buffer_handles_single_push_larger_than_capacity() {
        let mut rb = RingBuffer::new(4);
        rb.push(b"abcdefghij");
        assert_eq!(rb.dropped(), 6);
        let (bytes, _) = rb.drain();
        assert_eq!(bytes, b"ghij");
    }

    #[test]
    fn registry_assigns_unique_session_ids() {
        let r = Arc::new(PtySessionRegistry::new());
        let a = r.next_session_id();
        let b = r.next_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("pty-"));
    }
}
