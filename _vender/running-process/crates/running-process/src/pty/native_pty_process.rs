use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use super::backend::PtySlave;
use super::backend::{Backend, PtyBackend, PtyChild, PtyMaster, PtySize};
#[cfg(unix)]
use super::posix_terminal_input_relay_worker;
#[cfg(windows)]
use super::{
    apply_windows_pty_priority, assign_child_to_windows_kill_on_close_job,
    assign_conpty_conhost_to_job, conhost_children_of_current_process,
};
use super::{
    is_ignorable_process_control_error, poll_pty_process, record_pty_input_metrics,
    spawn_pty_reader, store_pty_returncode, write_pty_input, IdleDetectorCore, NativePtyHandles,
    PtyError, PtyReadShared, PtyReadState,
};

#[cfg(unix)]
use super::pty_posix as pty_platform;
#[cfg(windows)]
use super::pty_windows;

/// Low-level native pseudo-terminal process wrapper.
///
/// The process is configured at construction time and is spawned by
/// [`Self::start_impl`]. Output is collected by a reader thread and exposed
/// through the chunk-reading methods.
pub struct NativePtyProcess {
    /// Command argv, including the executable as the first element.
    pub argv: Vec<String>,
    /// Working directory used when spawning the child, or the current directory.
    pub cwd: Option<String>,
    /// Environment overrides passed to the child process.
    pub env: Option<Vec<(String, String)>>,
    /// Initial PTY row count.
    pub rows: u16,
    /// Initial PTY column count.
    pub cols: u16,
    /// Optional Windows process priority hint for the PTY child.
    #[cfg(windows)]
    pub nice: Option<i32>,
    /// Native PTY handles for the running child, present after start.
    pub handles: Arc<Mutex<Option<NativePtyHandles>>>,
    /// Shared reader queue and condition variable for PTY output.
    pub reader: Arc<PtyReadShared>,
    /// Cached child exit code once the process has exited.
    pub returncode: Arc<Mutex<Option<i32>>>,
    /// Total bytes written to the PTY input stream.
    pub input_bytes_total: Arc<AtomicUsize>,
    /// Count of input writes containing a newline.
    pub newline_events_total: Arc<AtomicUsize>,
    /// Count of explicit submit events recorded for PTY input.
    pub submit_events_total: Arc<AtomicUsize>,
    /// When true, the reader thread writes PTY output to stdout.
    pub echo: Arc<AtomicBool>,
    /// When set, the reader thread feeds output directly to the idle detector.
    pub idle_detector: Arc<Mutex<Option<Arc<IdleDetectorCore>>>>,
    /// Visible (non-control) output bytes seen by the reader thread.
    pub output_bytes_total: Arc<AtomicUsize>,
    /// Control churn bytes (ANSI escapes, BS, CR, DEL) seen by the reader.
    pub control_churn_bytes_total: Arc<AtomicUsize>,
    /// Background worker that drains PTY output into the shared queue.
    pub reader_worker: Mutex<Option<thread::JoinHandle<()>>>,
    /// Stop flag observed by the terminal input relay worker.
    pub terminal_input_relay_stop: Arc<AtomicBool>,
    /// Whether the terminal input relay worker is currently active.
    pub terminal_input_relay_active: Arc<AtomicBool>,
    /// Background worker that forwards local terminal input into the PTY.
    pub terminal_input_relay_worker: Mutex<Option<thread::JoinHandle<()>>>,
}

pub(super) fn resolved_spawn_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.map(str::to_owned).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().to_string())
    })
}

impl NativePtyProcess {
    /// Terminate and reap a Unix PTY process without allowing child or reader
    /// cleanup to block the caller indefinitely.
    #[cfg(unix)]
    pub(super) fn finish_unix_teardown(&self, handles: NativePtyHandles) -> Result<(), PtyError> {
        const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);
        const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
        const READER_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;
        let process_group = master.process_group_leader();

        let mut control_error = None;
        if let Some(pid) = process_group {
            if let Err(err) = crate::unix_signal_process_group(pid, crate::UnixSignal::Kill) {
                if !is_ignorable_process_control_error(&err) {
                    control_error = Some(err);
                }
            }
        }
        if let Err(err) = child.kill() {
            if !is_ignorable_process_control_error(&err) && control_error.is_none() {
                control_error = Some(err);
            }
        }
        drop(writer);

        let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status as i32,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(CHILD_POLL_INTERVAL);
                }
                Ok(None) => break -9,
                Err(err) => {
                    if control_error.is_none() {
                        control_error = Some(err);
                    }
                    break -9;
                }
            }
        };
        self.store_returncode(code);

        let reader_worker = self
            .reader_worker
            .lock()
            .expect("pty reader worker mutex poisoned")
            .take();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(master);
            drop(child);
            if let Some(worker) = reader_worker {
                let _ = worker.join();
            }
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(READER_TEARDOWN_TIMEOUT);
        self.mark_reader_closed();

        match control_error {
            Some(err) => Err(PtyError::Io(err)),
            None => Ok(()),
        }
    }

    /// Create a pseudo-terminal process configuration.
    ///
    /// The child is not spawned until [`Self::start_impl`] is called.
    pub fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Vec<(String, String)>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> Result<Self, PtyError> {
        if argv.is_empty() {
            return Err(PtyError::Other("command cannot be empty".into()));
        }
        #[cfg(not(windows))]
        let _ = nice;
        Ok(Self {
            argv,
            cwd,
            env,
            rows,
            cols,
            #[cfg(windows)]
            nice,
            handles: Arc::new(Mutex::new(None)),
            reader: Arc::new(PtyReadShared {
                state: Mutex::new(PtyReadState {
                    chunks: VecDeque::new(),
                    closed: false,
                }),
                condvar: Condvar::new(),
            }),
            returncode: Arc::new(Mutex::new(None)),
            input_bytes_total: Arc::new(AtomicUsize::new(0)),
            newline_events_total: Arc::new(AtomicUsize::new(0)),
            submit_events_total: Arc::new(AtomicUsize::new(0)),
            echo: Arc::new(AtomicBool::new(false)),
            idle_detector: Arc::new(Mutex::new(None)),
            output_bytes_total: Arc::new(AtomicUsize::new(0)),
            control_churn_bytes_total: Arc::new(AtomicUsize::new(0)),
            reader_worker: Mutex::new(None),
            terminal_input_relay_stop: Arc::new(AtomicBool::new(false)),
            terminal_input_relay_active: Arc::new(AtomicBool::new(false)),
            terminal_input_relay_worker: Mutex::new(None),
        })
    }

    /// Mark the reader stream closed and wake all waiting readers.
    pub fn mark_reader_closed(&self) {
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        guard.closed = true;
        self.reader.condvar.notify_all();
    }

    /// Store the process return code if it has been observed.
    pub fn store_returncode(&self, code: i32) {
        store_pty_returncode(&self.returncode, code);
    }

    /// Record PTY input byte, newline, and submit counters.
    pub fn record_input_metrics(&self, data: &[u8], submit: bool) {
        record_pty_input_metrics(
            &self.input_bytes_total,
            &self.newline_events_total,
            &self.submit_events_total,
            data,
            submit,
        );
    }

    /// Write bytes to the PTY input stream and record input metrics.
    pub fn write_impl(&self, data: &[u8], submit: bool) -> Result<(), PtyError> {
        self.record_input_metrics(data, submit);
        write_pty_input(&self.handles, data)?;
        Ok(())
    }

    /// Signal the terminal input relay worker to stop.
    pub fn request_terminal_input_relay_stop(&self) {
        self.terminal_input_relay_stop
            .store(true, Ordering::Release);
        self.terminal_input_relay_active
            .store(false, Ordering::Release);
    }

    /// Start forwarding local terminal input into the PTY.
    pub fn start_terminal_input_relay_impl(&self) -> Result<(), PtyError> {
        let mut worker_guard = self
            .terminal_input_relay_worker
            .lock()
            .expect("pty terminal input relay mutex poisoned");
        if worker_guard.is_some() && self.terminal_input_relay_active() {
            return Ok(());
        }
        if self
            .handles
            .lock()
            .expect("pty handles mutex poisoned")
            .is_none()
        {
            return Err(PtyError::NotRunning);
        }

        self.terminal_input_relay_stop
            .store(false, Ordering::Release);
        self.terminal_input_relay_active
            .store(true, Ordering::Release);

        let handles = Arc::clone(&self.handles);
        let returncode = Arc::clone(&self.returncode);
        let input_bytes_total = Arc::clone(&self.input_bytes_total);
        let newline_events_total = Arc::clone(&self.newline_events_total);
        let submit_events_total = Arc::clone(&self.submit_events_total);
        let stop = Arc::clone(&self.terminal_input_relay_stop);
        let active = Arc::clone(&self.terminal_input_relay_active);

        #[cfg(windows)]
        {
            let capture = super::terminal_input::TerminalInputCore::new();
            capture.start_impl().map_err(PtyError::Io)?;
            *worker_guard = Some(thread::spawn(move || {
                loop {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    match poll_pty_process(&handles, &returncode) {
                        Ok(Some(_)) => break,
                        Ok(None) => {}
                        Err(_) => break,
                    }
                    match super::terminal_input::wait_for_terminal_input_event(
                        &capture.state,
                        &capture.condvar,
                        Some(Duration::from_millis(50)),
                    ) {
                        super::terminal_input::TerminalInputWaitOutcome::Event(event) => {
                            record_pty_input_metrics(
                                &input_bytes_total,
                                &newline_events_total,
                                &submit_events_total,
                                &event.data,
                                event.submit,
                            );
                            if write_pty_input(&handles, &event.data).is_err() {
                                break;
                            }
                        }
                        super::terminal_input::TerminalInputWaitOutcome::Timeout => continue,
                        super::terminal_input::TerminalInputWaitOutcome::Closed => break,
                    }
                }
                active.store(false, Ordering::Release);
                let _ = capture.stop_impl();
            }));
            Ok(())
        }

        #[cfg(unix)]
        {
            if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
                self.terminal_input_relay_active
                    .store(false, Ordering::Release);
                return Ok(());
            }

            *worker_guard = Some(thread::spawn(move || {
                posix_terminal_input_relay_worker(
                    handles,
                    returncode,
                    input_bytes_total,
                    newline_events_total,
                    submit_events_total,
                    stop,
                    active,
                );
            }));
            Ok(())
        }
    }

    /// Stop the terminal input relay worker and wait for it to exit.
    pub fn stop_terminal_input_relay_impl(&self) {
        self.request_terminal_input_relay_stop();
        if let Some(worker) = self
            .terminal_input_relay_worker
            .lock()
            .expect("pty terminal input relay mutex poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }

    /// Return whether the terminal input relay worker is active.
    pub fn terminal_input_relay_active(&self) -> bool {
        self.terminal_input_relay_active.load(Ordering::Acquire)
    }

    /// Synchronously tear down the PTY and reap the child.
    #[inline(never)]
    pub fn close_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::close_impl");
        self.stop_terminal_input_relay_impl();
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return Ok(());
        };
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            child,
        } = handles;

        #[cfg(windows)]
        {
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.drop_job"
                );
                drop(_job);
            }

            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.wait_job_exit"
                );
                let wait_deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let code = status as i32;
                            self.store_returncode(code);
                            break;
                        }
                        Ok(None) if Instant::now() < wait_deadline => {
                            // #199: intentional — `PtyChild` doesn't
                            // expose a "wait with timeout" method
                            // (portable-pty's Child trait doesn't
                            // either). Polling at 10ms inside a
                            // bounded 2-second deadline is the
                            // close-path graceful-exit watcher.
                            thread::sleep(Duration::from_millis(10));
                        }
                        Ok(None) => {
                            if let Err(err) = child.kill() {
                                if !is_ignorable_process_control_error(&err) {
                                    return Err(PtyError::Io(err));
                                }
                            }
                            // Bounded wait after kill (issue #590, cluster I):
                            // `ConPtyChild::wait` is a raw
                            // `WaitForSingleObject(process, INFINITE)`, so an
                            // uninterruptible child that survives
                            // TerminateProcess would wedge close() forever.
                            // Poll `try_wait` for a short grace instead; the
                            // Job Object's KILL_ON_JOB_CLOSE (dropped earlier)
                            // plus TerminateProcess normally make this resolve
                            // in one iteration.
                            let kill_deadline = Instant::now() + Duration::from_secs(2);
                            let code = loop {
                                match child.try_wait() {
                                    Ok(Some(status)) => break status as i32,
                                    Ok(None) if Instant::now() < kill_deadline => {
                                        thread::sleep(Duration::from_millis(10));
                                    }
                                    _ => break -9,
                                }
                            };
                            self.store_returncode(code);
                            break;
                        }
                        Err(_) => {
                            self.store_returncode(-9);
                            break;
                        }
                    }
                }
            }
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.drop_writer"
                );
                drop(writer);
            }
            // Bounded teardown (issue #590, cluster C). `drop(master)`
            // calls ClosePseudoConsole, which blocks until the ConPTY
            // output pipe drains; the reader's synchronous ReadFile only
            // sees EOF once that pipe's last write end closes. A detached
            // grandchild that inherited the pipe keeps it open, so both
            // ClosePseudoConsole and the reader join would wedge close()
            // forever. Run them on a helper thread and bound the wait; on
            // timeout we return — the teardown thread + reader leak, but the
            // caller is unblocked.
            {
                crate::rp_rust_debug_scope!(
                    "running_process::NativePtyProcess::close_impl.bounded_teardown"
                );
                let reader_worker = self
                    .reader_worker
                    .lock()
                    .expect("pty reader worker mutex poisoned")
                    .take();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    drop(master);
                    drop(child);
                    if let Some(worker) = reader_worker {
                        let _ = worker.join();
                    }
                    let _ = tx.send(());
                });
                let _ = rx.recv_timeout(Duration::from_secs(2));
            }
            self.mark_reader_closed();
            Ok(())
        }

        #[cfg(not(windows))]
        {
            self.finish_unix_teardown(NativePtyHandles {
                master,
                writer,
                child,
            })
        }
    }

    /// Best-effort, non-blocking teardown for use from `Drop`.
    #[inline(never)]
    pub fn close_nonblocking(&self) {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::close_nonblocking");
        #[cfg(windows)]
        self.request_terminal_input_relay_stop();
        let Ok(mut guard) = self.handles.lock() else {
            return;
        };
        let Some(handles) = guard.take() else {
            self.mark_reader_closed();
            return;
        };
        drop(guard);

        #[cfg(windows)]
        let NativePtyHandles {
            master,
            writer,
            mut child,
            _job,
        } = handles;
        #[cfg(not(windows))]
        let NativePtyHandles {
            master,
            writer,
            mut child,
        } = handles;

        if let Err(err) = child.kill() {
            if !is_ignorable_process_control_error(&err) {
                return;
            }
        }
        drop(writer);
        // On Windows `drop(master)` (ClosePseudoConsole) blocks until the
        // ConPTY output pipe drains, which can wedge if a grandchild
        // inherited it (issue #590, cluster C). This is the `Drop` path and
        // MUST stay non-blocking as its name promises, so move the blocking
        // drops to a detached thread (`PtyMaster`/`PtyChild` are
        // `Send + 'static`). On Unix `drop(master)` just closes the master
        // fd, so drop inline.
        #[cfg(windows)]
        std::thread::spawn(move || {
            drop(master);
            drop(child);
            drop(_job);
        });
        #[cfg(not(windows))]
        {
            drop(master);
            drop(child);
        }
        self.mark_reader_closed();
    }

    /// Spawn the configured child process inside a native PTY.
    pub fn start_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::start");
        let mut guard = self.handles.lock().expect("pty handles mutex poisoned");
        if guard.is_some() {
            return Err(PtyError::AlreadyStarted);
        }

        // Snapshot our conhost.exe children before openpty() so we can diff
        // after spawn to find the new conhost.exe created by ConPTY.
        #[cfg(windows)]
        let conhost_pids_before = conhost_children_of_current_process();

        let (mut master, slave) = Backend::openpty(PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| PtyError::Spawn(e.to_string()))?;

        // Build argv/cwd/env in the shape the backend wants.
        let argv: Vec<std::ffi::OsString> =
            self.argv.iter().map(std::ffi::OsString::from).collect();
        let cwd = resolved_spawn_cwd(self.cwd.as_deref());
        let env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> =
            self.env.as_ref().map(|e| {
                e.iter()
                    .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)))
                    .collect()
            });

        let reader = master
            .try_clone_reader()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let writer = master
            .take_writer()
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        let cwd_path = cwd.as_deref().map(std::path::Path::new);
        let child = slave
            .spawn(&argv, cwd_path, env.as_deref())
            .map_err(|e| PtyError::Spawn(e.to_string()))?;
        // The trait's PtyChild::as_raw_handle returns Option<RawHandle>
        // matching portable_pty's signature; pass directly.
        #[cfg(windows)]
        let job = assign_child_to_windows_kill_on_close_job(PtyChild::as_raw_handle(&child))?;
        #[cfg(windows)]
        assign_conpty_conhost_to_job(&job, &conhost_pids_before);
        #[cfg(windows)]
        apply_windows_pty_priority(PtyChild::as_raw_handle(&child), self.nice)?;
        let shared = Arc::clone(&self.reader);
        let echo = Arc::clone(&self.echo);
        let idle_detector = Arc::clone(&self.idle_detector);
        let output_bytes = Arc::clone(&self.output_bytes_total);
        let churn_bytes = Arc::clone(&self.control_churn_bytes_total);
        let reader_worker = thread::spawn(move || {
            spawn_pty_reader(
                reader,
                shared,
                echo,
                idle_detector,
                output_bytes,
                churn_bytes,
            );
        });
        *self
            .reader_worker
            .lock()
            .expect("pty reader worker mutex poisoned") = Some(reader_worker);

        *guard = Some(NativePtyHandles {
            master: Box::new(master) as Box<dyn PtyMaster>,
            // #590 cluster D: writer lives behind its own mutex so a
            // blocking input write never holds the `handles` lock.
            writer: Arc::new(Mutex::new(writer)),
            child: Box::new(child) as Box<dyn PtyChild>,
            #[cfg(windows)]
            _job: job,
        });
        Ok(())
    }

    /// Respond to terminal query escape sequences found in a PTY output chunk.
    pub fn respond_to_queries_impl(&self, data: &[u8]) -> Result<(), PtyError> {
        #[cfg(windows)]
        {
            pty_windows::respond_to_queries(self, data)
        }

        #[cfg(unix)]
        {
            pty_platform::respond_to_queries(self, data)
        }
    }

    /// Resize the PTY to the given row and column dimensions.
    pub fn resize_impl(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::resize");
        let guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_ref() {
            #[cfg(windows)]
            {
                let _ = (rows, cols, handles);
                // ConPTY resize can leave ClosePseudoConsole blocked during
                // teardown on Windows. Keep resize as a no-op until the
                // backend can cancel the outstanding PTY read safely.
                return Ok(());
            }

            #[cfg(not(windows))]
            handles
                .master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| PtyError::Other(e.to_string()))?;
        }
        Ok(())
    }

    /// Send an interrupt signal or control event to the PTY child.
    pub fn send_interrupt_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::send_interrupt");
        #[cfg(windows)]
        {
            pty_windows::send_interrupt(self)
        }

        #[cfg(unix)]
        {
            pty_platform::send_interrupt(self)
        }
    }

    /// Wait for the PTY child to exit and return its exit code.
    ///
    /// Returns a timeout error when `timeout` elapses before exit.
    pub fn wait_impl(&self, timeout: Option<f64>) -> Result<i32, PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::wait");
        // Fast path: already exited.
        if let Some(code) = *self
            .returncode
            .lock()
            .expect("pty returncode mutex poisoned")
        {
            return Ok(code);
        }
        let start = Instant::now();
        loop {
            if let Some(code) = poll_pty_process(&self.handles, &self.returncode)? {
                return Ok(code);
            }
            if timeout.is_some_and(|limit| start.elapsed() >= Duration::from_secs_f64(limit)) {
                return Err(PtyError::Timeout);
            }
            // #199: intentional — `wait_impl` poll. Same constraint
            // as the close_impl variant above: no per-Child wait
            // primitive on the trait surface.
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Request graceful termination of the PTY child.
    pub fn terminate_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::terminate");
        #[cfg(windows)]
        {
            if self
                .handles
                .lock()
                .expect("pty handles mutex poisoned")
                .is_none()
            {
                return Err(PtyError::NotRunning);
            }
            self.close_impl()
        }

        #[cfg(unix)]
        {
            pty_platform::terminate(self)
        }
    }

    /// Forcefully terminate the PTY child.
    pub fn kill_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::kill");
        #[cfg(windows)]
        {
            if self
                .handles
                .lock()
                .expect("pty handles mutex poisoned")
                .is_none()
            {
                return Err(PtyError::NotRunning);
            }
            self.close_impl()
        }

        #[cfg(unix)]
        {
            pty_platform::kill(self)
        }
    }

    /// Request graceful termination of the PTY child process tree.
    pub fn terminate_tree_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::terminate_tree");
        #[cfg(windows)]
        {
            pty_windows::terminate_tree(self)
        }

        #[cfg(unix)]
        {
            pty_platform::terminate_tree(self)
        }
    }

    /// Forcefully terminate the PTY child process tree.
    pub fn kill_tree_impl(&self) -> Result<(), PtyError> {
        crate::rp_rust_debug_scope!("running_process::NativePtyProcess::kill_tree");
        #[cfg(windows)]
        {
            pty_windows::kill_tree(self)
        }

        #[cfg(unix)]
        {
            pty_platform::kill_tree(self)
        }
    }

    /// Get the PID of the child process, if running.
    pub fn pid(&self) -> Result<Option<u32>, PtyError> {
        let guard = self.handles.lock().expect("pty handles mutex poisoned");
        if let Some(handles) = guard.as_ref() {
            #[cfg(unix)]
            if let Some(pid) = handles.master.process_group_leader() {
                if let Ok(pid) = u32::try_from(pid) {
                    return Ok(Some(pid));
                }
            }
            return Ok(Some(handles.child.pid()));
        }
        Ok(None)
    }

    /// Wait for a chunk of output from the PTY reader.
    /// Returns `Ok(Some(chunk))` on data, `Ok(None)` on timeout, `Err` on closed.
    pub fn read_chunk_impl(&self, timeout: Option<f64>) -> Result<Option<Vec<u8>>, PtyError> {
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        loop {
            if let Some(chunk) = guard.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            if guard.closed {
                return Err(PtyError::Other("Pseudo-terminal stream is closed".into()));
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None); // timeout
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .reader
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("pty read mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = self
                        .reader
                        .condvar
                        .wait(guard)
                        .expect("pty read mutex poisoned");
                }
            }
        }
    }

    /// Wait for the reader thread to close.
    pub fn wait_for_reader_closed_impl(&self, timeout: Option<f64>) -> bool {
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        loop {
            if guard.closed {
                return true;
            }
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    let wait = deadline.saturating_duration_since(now);
                    let result = self
                        .reader
                        .condvar
                        .wait_timeout(guard, wait)
                        .expect("pty read mutex poisoned");
                    guard = result.0;
                }
                None => {
                    guard = self
                        .reader
                        .condvar
                        .wait(guard)
                        .expect("pty read mutex poisoned");
                }
            }
        }
    }

    /// Wait for exit then drain remaining output.
    pub fn wait_and_drain_impl(
        &self,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> Result<i32, PtyError> {
        let code = self.wait_impl(timeout)?;
        let deadline = Instant::now() + Duration::from_secs_f64(drain_timeout.max(0.0));
        let mut guard = self.reader.state.lock().expect("pty read mutex poisoned");
        while !guard.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let result = self
                .reader
                .condvar
                .wait_timeout(guard, remaining)
                .expect("pty read mutex poisoned");
            guard = result.0;
        }
        Ok(code)
    }

    /// Enable or disable echoing PTY output to stdout.
    pub fn set_echo(&self, enabled: bool) {
        self.echo.store(enabled, Ordering::Release);
    }

    /// Return whether PTY output echoing is enabled.
    pub fn echo_enabled(&self) -> bool {
        self.echo.load(Ordering::Acquire)
    }

    /// Attach an idle detector that observes reader-thread output.
    pub fn attach_idle_detector(&self, detector: &Arc<IdleDetectorCore>) {
        let mut guard = self
            .idle_detector
            .lock()
            .expect("idle detector mutex poisoned");
        *guard = Some(Arc::clone(detector));
    }

    /// Detach the current idle detector, if one is attached.
    pub fn detach_idle_detector(&self) {
        let mut guard = self
            .idle_detector
            .lock()
            .expect("idle detector mutex poisoned");
        *guard = None;
    }

    /// Return total bytes written to PTY input.
    pub fn pty_input_bytes_total(&self) -> usize {
        self.input_bytes_total.load(Ordering::Acquire)
    }

    /// Return the number of PTY input writes containing newlines.
    pub fn pty_newline_events_total(&self) -> usize {
        self.newline_events_total.load(Ordering::Acquire)
    }

    /// Return the number of recorded PTY input submit events.
    pub fn pty_submit_events_total(&self) -> usize {
        self.submit_events_total.load(Ordering::Acquire)
    }

    /// Return visible PTY output bytes observed by the reader thread.
    pub fn pty_output_bytes_total(&self) -> usize {
        self.output_bytes_total.load(Ordering::Acquire)
    }

    /// Return control-churn bytes observed by the reader thread.
    pub fn pty_control_churn_bytes_total(&self) -> usize {
        self.control_churn_bytes_total.load(Ordering::Acquire)
    }
}

/// Safe defaults for a real interactive PTY session.
///
/// The helper turns on the parts that a terminal-style session usually needs:
/// output echo, terminal input relay, and automatic PTY query replies.
#[derive(Debug, Clone, Copy)]
pub struct InteractivePtyOptions {
    /// Echo PTY output to stdout while the session is running.
    pub echo_output: bool,
    /// Relay local terminal input into the PTY.
    pub relay_terminal_input: bool,
    /// Automatically answer terminal query escape sequences.
    pub respond_to_queries: bool,
}

impl Default for InteractivePtyOptions {
    fn default() -> Self {
        Self {
            echo_output: true,
            relay_terminal_input: true,
            respond_to_queries: true,
        }
    }
}

/// Output collected by one interactive PTY pump operation.
#[derive(Debug, Default)]
pub struct InteractivePtyPumpResult {
    /// Output chunks read from the PTY.
    pub chunks: Vec<Vec<u8>>,
    /// Whether the PTY stream closed while pumping output.
    pub stream_closed: bool,
}

/// Canonical interactive PTY recipe for downstream Rust consumers.
///
/// `NativePtyProcess` remains the low-level primitive. This wrapper owns the
/// interactive setup that callers commonly forget to assemble correctly.
pub struct InteractivePtySession {
    process: NativePtyProcess,
    options: InteractivePtyOptions,
}

impl InteractivePtySession {
    /// Create an interactive PTY session with default options.
    pub fn new(process: NativePtyProcess) -> Self {
        Self::with_options(process, InteractivePtyOptions::default())
    }

    /// Create an interactive PTY session with explicit options.
    pub fn with_options(process: NativePtyProcess, options: InteractivePtyOptions) -> Self {
        Self { process, options }
    }

    /// Return the wrapped low-level PTY process.
    pub fn process(&self) -> &NativePtyProcess {
        &self.process
    }

    /// Start the wrapped PTY process and configured interactive helpers.
    pub fn start(&self) -> Result<(), PtyError> {
        self.process.set_echo(self.options.echo_output);
        self.process.start_impl()?;
        if self.options.relay_terminal_input {
            self.process.start_terminal_input_relay_impl()?;
        }
        Ok(())
    }

    /// Read and optionally drain available PTY output.
    ///
    /// When query responses are enabled, terminal queries in each chunk are
    /// answered before the chunk is returned.
    pub fn pump_output(
        &self,
        timeout: Option<f64>,
        consume_all: bool,
    ) -> Result<InteractivePtyPumpResult, PtyError> {
        let mut pumped = InteractivePtyPumpResult::default();
        let mut next_timeout = timeout;
        loop {
            match self.process.read_chunk_impl(next_timeout) {
                Ok(Some(chunk)) => {
                    if self.options.respond_to_queries {
                        self.process.respond_to_queries_impl(&chunk)?;
                    }
                    pumped.chunks.push(chunk);
                    if !consume_all {
                        break;
                    }
                    next_timeout = Some(0.0);
                }
                Ok(None) => break,
                Err(PtyError::Other(message)) if message == "Pseudo-terminal stream is closed" => {
                    pumped.stream_closed = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(pumped)
    }

    /// Resize the interactive PTY.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), PtyError> {
        self.process.resize_impl(rows, cols)
    }

    /// Send an interrupt to the interactive PTY child.
    pub fn send_interrupt(&self) -> Result<(), PtyError> {
        self.process.send_interrupt_impl()
    }

    /// Wait for the interactive PTY child to exit.
    pub fn wait(&self, timeout: Option<f64>) -> Result<i32, PtyError> {
        self.process.wait_impl(timeout)
    }

    /// Wait for the child to exit, then drain remaining PTY output.
    pub fn wait_and_drain(
        &self,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> Result<i32, PtyError> {
        self.process.wait_and_drain_impl(timeout, drain_timeout)
    }

    /// Request graceful termination of the interactive PTY child.
    pub fn terminate(&self) -> Result<(), PtyError> {
        self.process.terminate_impl()
    }

    /// Forcefully terminate the interactive PTY child.
    pub fn kill(&self) -> Result<(), PtyError> {
        self.process.kill_impl()
    }

    /// Close the interactive PTY session.
    pub fn close(&self) -> Result<(), PtyError> {
        self.process.close_impl()
    }
}

impl Drop for NativePtyProcess {
    fn drop(&mut self) {
        self.close_nonblocking();
    }
}
