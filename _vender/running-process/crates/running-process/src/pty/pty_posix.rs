use super::*;
use crate::{unix_signal_process, unix_signal_process_group, UnixSignal};
use std::io;
use std::os::fd::RawFd;
use std::sync::TryLockError;
use sysinfo::{Pid, System};

fn system_pid(pid: u32) -> Pid {
    Pid::from_u32(pid)
}

fn descendant_pids(system: &System, pid: Pid) -> Vec<Pid> {
    use std::collections::HashMap;
    let mut children_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (child_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children_map.entry(parent).or_default().push(*child_pid);
        }
    }
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some(children) = children_map.get(&current) {
            for &child in children {
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

fn signal_tree(pid: u32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let system = System::new_all();
    let pid = system_pid(pid);
    let Some(_) = system.process(pid) else {
        return Ok(());
    };

    let mut targets = descendant_pids(&system, pid);
    targets.reverse();
    targets.push(pid);

    for target in targets {
        let raw_pid = target.as_u32();
        if let Err(err) = unix_signal_process(raw_pid, signal) {
            if !is_ignorable_process_control_error(&err) {
                return Err(err);
            }
        }
    }
    Ok(())
}

pub(super) fn input_payload(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}

pub(super) fn respond_to_queries(
    _process: &NativePtyProcess,
    _data: &[u8],
) -> Result<(), PtyError> {
    Ok(())
}

pub(super) fn send_interrupt(process: &NativePtyProcess) -> Result<(), PtyError> {
    let guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_ref().ok_or(PtyError::NotRunning)?;
    if let Some(pid) = handles.master.process_group_leader() {
        unix_signal_process_group(pid, UnixSignal::Interrupt)?;
        return Ok(());
    }

    // The raw Ctrl-C fallback is explicitly best-effort. Never wait behind an
    // ordinary input write that is already blocked on a full PTY queue.
    let _writer_guard = match handles.writer.try_lock() {
        Ok(writer) => writer,
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Poisoned(_)) => {
            return Err(PtyError::Other("pty writer mutex poisoned".into()))
        }
    };
    let master_fd = handles
        .master
        .as_raw_fd()
        .ok_or_else(|| PtyError::Other("PTY master does not expose a Unix descriptor".into()))?;
    process.record_input_metrics(&[0x03], false);
    write_nonblocking_byte(master_fd, 0x03)?;
    Ok(())
}

struct FdFlagsGuard {
    fd: RawFd,
    original_flags: libc::c_int,
    restored: bool,
}

fn get_fd_flags(fd: RawFd) -> io::Result<libc::c_int> {
    loop {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags != -1 {
            return Ok(flags);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn set_fd_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    loop {
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

impl FdFlagsGuard {
    fn make_nonblocking(fd: RawFd) -> io::Result<Self> {
        let original_flags = get_fd_flags(fd)?;
        set_fd_flags(fd, original_flags | libc::O_NONBLOCK)?;
        Ok(Self {
            fd,
            original_flags,
            restored: false,
        })
    }

    fn restore(mut self) -> io::Result<()> {
        set_fd_flags(self.fd, self.original_flags)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for FdFlagsGuard {
    fn drop(&mut self) {
        if !self.restored {
            let _ = set_fd_flags(self.fd, self.original_flags);
        }
    }
}

fn write_nonblocking_byte(fd: RawFd, byte: u8) -> io::Result<()> {
    let flags = FdFlagsGuard::make_nonblocking(fd)?;
    let written = unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    let write_result = if written == 1 {
        Ok(())
    } else if written == -1 {
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "PTY interrupt fallback wrote zero bytes",
        ))
    };
    let restore_result = flags.restore();
    restore_result.and(write_result)
}

pub(super) fn terminate(process: &NativePtyProcess) -> Result<(), PtyError> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.as_mut().ok_or(PtyError::NotRunning)?;
    let pid = handles.child.pid();
    if pid == 0 {
        return Err(PtyError::NotRunning);
    }
    unix_signal_process(pid, UnixSignal::Terminate)?;
    Ok(())
}

pub(super) fn kill(process: &NativePtyProcess) -> Result<(), PtyError> {
    let mut guard = process.handles.lock().expect("pty handles mutex poisoned");
    let handles = guard.take().ok_or(PtyError::NotRunning)?;
    drop(guard);
    process.finish_unix_teardown(handles)
}

pub(super) fn terminate_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    let pid = process.pid()?.ok_or(PtyError::NotRunning)?;
    signal_tree(pid, UnixSignal::Terminate)?;
    Ok(())
}

pub(super) fn kill_tree(process: &NativePtyProcess) -> Result<(), PtyError> {
    let pid = process.pid()?.ok_or(PtyError::NotRunning)?;
    signal_tree(pid, UnixSignal::Kill)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::backend::{PtyChild, PtyMaster, PtySize};
    use crate::pty::NativePtyHandles;
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct NoGroupMaster {
        _fd: OwnedFd,
    }

    impl PtyMaster for NoGroupMaster {
        fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unused by test"))
        }

        fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "unused by test"))
        }

        fn resize(&self, _size: PtySize) -> io::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> io::Result<PtySize> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
        }

        fn process_group_leader(&self) -> Option<i32> {
            None
        }

        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            Some(self._fd.as_raw_fd())
        }
    }

    struct RunningChild;

    impl PtyChild for RunningChild {
        fn pid(&self) -> u32 {
            1
        }

        fn try_wait(&mut self) -> io::Result<Option<u32>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<u32> {
            Ok(0)
        }

        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn open_full_pty_input_queue() -> (OwnedFd, OwnedFd) {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0,
            "openpty failed: {}",
            io::Error::last_os_error()
        );
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };

        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), termios.as_mut_ptr()) },
            0
        );
        let mut termios = unsafe { termios.assume_init() };
        unsafe { libc::cfmakeraw(&mut termios) };
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &termios) },
            0
        );

        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK,) },
            -1
        );
        let chunk = [b'x'; 1024];
        loop {
            let written =
                unsafe { libc::write(master.as_raw_fd(), chunk.as_ptr().cast(), chunk.len()) };
            if written >= 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            break;
        }
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags) },
            -1
        );
        (master, slave)
    }

    #[test]
    // Regression for #618: the fallback itself must finish before this test
    // drains the deliberately full, non-reading PTY input queue.
    fn interrupt_fallback_does_not_block_on_full_pty_input_queue() {
        const DEADLINE: Duration = Duration::from_secs(1);

        let (master, slave) = open_full_pty_input_queue();
        let writer_fd = unsafe { libc::dup(master.as_raw_fd()) };
        assert_ne!(writer_fd, -1);
        let writer = unsafe { File::from_raw_fd(writer_fd) };

        let process = NativePtyProcess::new(vec!["unused".into()], None, None, 24, 80, None)
            .expect("test process");
        *process.handles.lock().expect("handles mutex") = Some(NativePtyHandles {
            master: Box::new(NoGroupMaster { _fd: master }),
            writer: Arc::new(Mutex::new(Box::new(writer))),
            child: Box::new(RunningChild),
        });

        let (ready_tx, ready_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let result = send_interrupt(&process);
            let _ = tx.send(result);
            let _ = process.handles.lock().expect("handles mutex").take();
        });

        ready_rx
            .recv_timeout(DEADLINE)
            .expect("interrupt worker did not start");
        let started = Instant::now();
        let timely = rx.recv_timeout(DEADLINE);
        let result = match timely {
            Ok(result) => result,
            Err(_) => {
                let mut drain = [0u8; 1024];
                assert!(
                    unsafe {
                        libc::read(slave.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len())
                    } > 0,
                    "failed to drain the full PTY queue"
                );
                rx.recv_timeout(Duration::from_secs(1))
                    .expect("interrupt fallback did not unblock after draining the PTY queue")
            }
        };
        let elapsed = started.elapsed();
        worker.join().expect("interrupt worker panicked");

        assert!(
            elapsed < DEADLINE,
            "interrupt fallback blocked for {elapsed:?} on a full PTY input queue"
        );
        assert!(result.is_ok(), "best-effort fallback failed: {result:?}");
    }

    #[test]
    fn nonblocking_interrupt_write_restores_descriptor_flags() {
        let (master, slave) = open_full_pty_input_queue();
        let before = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(before, -1);

        write_nonblocking_byte(master.as_raw_fd(), 0x03)
            .expect("a full input queue is an expected best-effort drop");

        let after = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(after, before, "fallback changed the PTY descriptor flags");
        drop(slave);
    }

    #[test]
    fn nonblocking_interrupt_write_reports_fcntl_failure() {
        let error = write_nonblocking_byte(-1, 0x03).expect_err("invalid fd must fail");
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));

        let guard = FdFlagsGuard {
            fd: -1,
            original_flags: 0,
            restored: false,
        };
        let restore_error = guard.restore().expect_err("invalid fd restore must fail");
        assert_eq!(restore_error.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn interrupt_fallback_does_not_wait_for_busy_writer_mutex() {
        let (master, slave) = open_full_pty_input_queue();
        let writer_fd = unsafe { libc::dup(master.as_raw_fd()) };
        assert_ne!(writer_fd, -1);
        let writer = Arc::new(Mutex::new(
            Box::new(unsafe { File::from_raw_fd(writer_fd) }) as Box<dyn Write + Send>,
        ));

        let process = NativePtyProcess::new(vec!["unused".into()], None, None, 24, 80, None)
            .expect("test process");
        *process.handles.lock().expect("handles mutex") = Some(NativePtyHandles {
            master: Box::new(NoGroupMaster { _fd: master }),
            writer: Arc::clone(&writer),
            child: Box::new(RunningChild),
        });

        let writer_guard = writer.lock().expect("writer mutex");
        let (ready_tx, ready_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let result = send_interrupt(&process);
            let _ = tx.send(result);
            let _ = process.handles.lock().expect("handles mutex").take();
        });

        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupt worker did not start");
        let result = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupt fallback waited for the busy writer mutex");
        assert!(result.is_ok(), "best-effort fallback failed: {result:?}");
        drop(writer_guard);
        drop(slave);
        worker.join().expect("interrupt worker panicked");
    }
}
