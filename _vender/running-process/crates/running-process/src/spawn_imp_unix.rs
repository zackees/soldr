use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::helpers::{kill_drain_deadline, poll_until};

trait UnixChild: Send {
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<std::process::ExitStatus>;
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>>;
}

impl UnixChild for std::process::Child {
    fn kill(&mut self) -> io::Result<()> {
        std::process::Child::kill(self)
    }

    fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        std::process::Child::wait(self)
    }

    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        std::process::Child::try_wait(self)
    }
}

pub struct SpawnedInner {
    child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
    pgid: i32,
}

impl SpawnedInner {
    pub fn kill(&self) -> io::Result<()> {
        // Try the child first, then the process group, to make sure
        // any siblings spawned inside go down too.
        let mut guard = self.child.lock().expect("child mutex poisoned");
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
        drop(guard);
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        Ok(())
    }

    pub fn wait(&self) -> io::Result<i32> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Err(io::Error::other("child handle absent"));
        };
        let status = child.wait()?;
        Ok(super::unix_exit_code(status))
    }

    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        let mut guard = self.child.lock().expect("child mutex poisoned");
        let Some(child) = guard.as_mut() else {
            return Ok(None);
        };
        Ok(child.try_wait()?.map(super::unix_exit_code))
    }

    pub fn shutdown(&mut self) {
        self.shutdown_with_deadline(kill_drain_deadline());
    }

    fn shutdown_with_deadline(&mut self, deadline: Instant) {
        let group_signaled = unsafe { libc::killpg(self.pgid, libc::SIGKILL) == 0 };
        let Some(mut child) = self.child.lock().expect("child mutex poisoned").take() else {
            return;
        };
        if !group_signaled {
            let _ = child.kill();
        }
        match poll_until(deadline, Duration::from_millis(10), || child.try_wait()) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => spawn_background_reaper(child),
        }
    }
}

fn spawn_background_reaper(mut child: Box<dyn UnixChild>) {
    thread::spawn(move || {
        // Once ownership is off the caller's teardown path, a blocking wait is
        // the most reliable terminal policy: it reaps exactly once without a
        // retry loop, spinning, or retaining the shared child mutex.
        let _ = child.wait();
    });
}

fn slot_to_stdio(slot: &super::StdioSource<'_>) -> io::Result<Stdio> {
    match slot {
        super::StdioSource::Null => Ok(Stdio::null()),
        super::StdioSource::Parent => Ok(Stdio::inherit()),
        super::StdioSource::Fd(fd) => {
            let owned = fd.try_clone_to_owned()?;
            Ok(Stdio::from(owned))
        }
        super::StdioSource::Pipe => Ok(Stdio::piped()),
        super::StdioSource::_Phantom(_) => unreachable!(),
    }
}

fn daemon_slot_to_stdio(slot: &super::DaemonStdioSource<'_>) -> io::Result<Stdio> {
    match slot {
        super::DaemonStdioSource::Null => Ok(Stdio::null()),
        super::DaemonStdioSource::Fd(fd) => Ok(Stdio::from(fd.try_clone_to_owned()?)),
        super::DaemonStdioSource::_Phantom(_) => unreachable!(),
    }
}

pub fn spawn_daemon(
    command: &mut Command,
    stdio: super::DaemonStdio<'_>,
    policy: super::EnvironmentPolicy,
) -> io::Result<super::DaemonChild> {
    use std::os::unix::process::CommandExt;

    apply_environment_policy(command, policy)?;

    command
        .stdin(Stdio::null())
        .stdout(daemon_slot_to_stdio(&stdio.stdout)?)
        .stderr(daemon_slot_to_stdio(&stdio.stderr)?);

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                // Already a session leader — not fatal.
            }
            close_extra_fds();
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child.id();
    Ok(super::DaemonChild { pid, child })
}

pub fn spawn(
    command: &mut Command,
    stdio: super::SpawnStdio<'_>,
    policy: super::EnvironmentPolicy,
) -> io::Result<super::SpawnedChild> {
    use std::os::unix::process::CommandExt;

    apply_environment_policy(command, policy)?;
    command.stdin(slot_to_stdio(&stdio.stdin)?);
    command.stdout(slot_to_stdio(&stdio.stdout)?);
    command.stderr(slot_to_stdio(&stdio.stderr)?);

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    libc::_exit(1);
                }
            }
            close_extra_fds();
            Ok(())
        });
    }

    let mut child = command.spawn()?;
    let pid = child.id();
    let pgid = pid as i32;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> = Arc::new(Mutex::new(Some(Box::new(child))));

    // Drain watcher: wait for exit, then sleep `drain_timeout`. We
    // don't proactively close anything on Unix — Rust's ChildStdin/etc.
    // own their fds; once the child exits and the kernel ref-counts
    // its copies to zero, parent reads will EOF naturally.
    if let Some(timeout) = stdio.drain_timeout {
        let child_clone = Arc::clone(&child);
        thread::spawn(move || {
            // Borrow child for try_wait.  We do a polling loop so
            // shutdown() taking the inner Child during Drop doesn't
            // wedge us.
            loop {
                {
                    let mut guard = child_clone.lock().expect("child mutex poisoned");
                    match guard.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(_) => break,
                        },
                        None => return,
                    }
                }
                // #199: intentional — try_wait poll on the contained
                // child, 50ms cadence inside a bounded outer drain
                // loop. waitpid(WNOHANG)-equivalent semantics.
                thread::sleep(std::time::Duration::from_millis(50));
            }
            // #199: intentional — post-mortem pipe drain. Children's
            // write-ends of the captured stdio pipes are still being
            // closed by the kernel after exit; this gives readers a
            // chance to see the final bytes before the watcher
            // releases its keep-alive.
            thread::sleep(timeout);
        });
    }

    Ok(super::SpawnedChild {
        stdin,
        stdout,
        stderr,
        pid,
        inner: SpawnedInner { child, pgid },
    })
}

fn apply_environment_policy(
    command: &mut Command,
    policy: super::EnvironmentPolicy,
) -> io::Result<()> {
    match policy {
        super::EnvironmentPolicy::Inherit => return Ok(()),
        super::EnvironmentPolicy::Auto => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Auto environment policy must be resolved before platform spawn",
            ));
        }
        super::EnvironmentPolicy::Clear | super::EnvironmentPolicy::UserBaseline => {
            // Changing the base with env_clear() also clears Command's
            // explicit mutation map. Snapshot additions, overrides, and
            // removals so they can be replayed after the selected base.
            let explicit: Vec<_> = command
                .get_envs()
                .map(|(key, value)| (key.to_os_string(), value.map(std::ffi::OsStr::to_os_string)))
                .collect();
            let baseline = match policy {
                super::EnvironmentPolicy::UserBaseline => {
                    crate::environment::user_baseline_environment()?
                }
                super::EnvironmentPolicy::Clear => Vec::new(),
                _ => unreachable!(),
            };

            command.env_clear();
            command.envs(baseline);
            for (key, value) in explicit {
                match value {
                    Some(value) => {
                        command.env(key, value);
                    }
                    None => {
                        command.env_remove(key);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Async-signal-safe fd sweep used in pre_exec. See sanitized.rs (now
/// merged here) for the rationale.
///
/// # Marked close-on-exec, not closed
///
/// The goal is that the child inherits nothing past `exec`, and `FD_CLOEXEC`
/// delivers exactly that — the kernel closes the descriptors as part of the
/// `exec` itself.
///
/// Closing them here instead used to break a protocol we do not own. Rust's
/// `Command::spawn` reports `exec` failure through a `CLOEXEC` pipe: the child
/// writes its `errno` to it, and the parent turns that into
/// `Err(io::Error)`. That pipe is an fd ≥ 3, so this sweep closed it. When
/// `exec` then failed — a mistyped program path being the ordinary case — the
/// child could not report why, and std's internal
/// `assert!(output.write(&bytes).is_ok())` aborted it. The caller saw a child
/// that died with `SIGABRT` rather than `Err(NotFound)`, and the two demand
/// completely different responses from an operator. See #716.
///
/// `CLOEXEC` keeps the descriptors usable for the instant between here and
/// `exec`, which is all std needs, while still guaranteeing the child that
/// actually starts inherits none of them.
unsafe fn close_extra_fds() {
    #[cfg(target_os = "linux")]
    {
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "powerpc64",
        ))]
        {
            const SYS_CLOSE_RANGE: libc::c_long = 436;
            // CLOSE_RANGE_CLOEXEC (Linux 5.11+): mark the range close-on-exec
            // instead of closing it now.
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 4;
            let rc = libc::syscall(
                SYS_CLOSE_RANGE,
                3u32,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            );
            if rc == 0 {
                return;
            }
            // Older kernels reject the flag (EINVAL); fall through to the
            // per-descriptor sweep rather than closing the range outright.
        }
    }

    let dir = libc::opendir(c"/dev/fd".as_ptr());
    if !dir.is_null() {
        let dir_fd = libc::dirfd(dir);
        loop {
            let ent = libc::readdir(dir);
            if ent.is_null() {
                break;
            }
            let name_ptr = (*ent).d_name.as_ptr();
            let mut fd: libc::c_int = 0;
            let mut p = name_ptr;
            let mut ok = false;
            while *p != 0 {
                let c = *p as u8;
                if !c.is_ascii_digit() {
                    ok = false;
                    break;
                }
                fd = fd * 10 + (c - b'0') as libc::c_int;
                p = p.add(1);
                ok = true;
            }
            if !ok {
                continue;
            }
            if fd > 2 && fd != dir_fd {
                set_cloexec(fd);
            }
        }
        libc::closedir(dir);
        return;
    }

    let max = libc::sysconf(libc::_SC_OPEN_MAX);
    let max = if max < 0 { 4096 } else { max as libc::c_int };
    for fd in 3..max {
        set_cloexec(fd);
    }
}

/// Mark one descriptor close-on-exec, preserving any other flags.
///
/// `fcntl` is async-signal-safe, which matters because this runs in the
/// forked child. Failures are ignored: the descriptor may simply not be open,
/// which is the common case in a sweep over a range.
unsafe fn set_cloexec(fd: libc::c_int) {
    let flags = libc::fcntl(fd, libc::F_GETFD);
    if flags == -1 {
        return;
    }
    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Condvar};

    struct FakeChild {
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
    }

    impl UnixChild for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            let (lock, condvar) = &*self.wait_gate;
            let mut released = lock.lock().expect("wait gate mutex poisoned");
            while !*released {
                released = condvar.wait(released).expect("wait gate mutex poisoned");
            }
            Ok(std::process::ExitStatus::from_raw(0))
        }

        fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            let released = *self.wait_gate.0.lock().expect("wait gate mutex poisoned");
            Ok(released.then(|| std::process::ExitStatus::from_raw(0)))
        }
    }

    struct BlockedFixture {
        inner: SpawnedInner,
        child: Arc<Mutex<Option<Box<dyn UnixChild>>>>,
        wait_gate: Arc<(Mutex<bool>, Condvar)>,
        waits: Arc<AtomicUsize>,
        kills: Arc<AtomicUsize>,
    }

    fn blocked_inner() -> BlockedFixture {
        let wait_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let waits = Arc::new(AtomicUsize::new(0));
        let kills = Arc::new(AtomicUsize::new(0));
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> =
            Arc::new(Mutex::new(Some(Box::new(FakeChild {
                wait_gate: Arc::clone(&wait_gate),
                waits: Arc::clone(&waits),
                kills: Arc::clone(&kills),
            }))));
        BlockedFixture {
            inner: SpawnedInner {
                child: Arc::clone(&child),
                pgid: i32::MAX,
            },
            child,
            wait_gate,
            waits,
            kills,
        }
    }

    fn release_wait(wait_gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, condvar) = &**wait_gate;
        *lock.lock().expect("wait gate mutex poisoned") = true;
        condvar.notify_all();
    }

    struct ShutdownOnDrop {
        inner: Option<SpawnedInner>,
        deadline: Instant,
    }

    impl Drop for ShutdownOnDrop {
        fn drop(&mut self) {
            self.inner
                .as_mut()
                .expect("test wrapper missing inner")
                .shutdown_with_deadline(self.deadline);
        }
    }

    #[test]
    fn drop_is_bounded_when_child_wait_does_not_complete() {
        // Regression for #619: SpawnedChild::drop delegates directly to
        // SpawnedInner::shutdown, modeled by this wrapper around the fake child.
        let BlockedFixture {
            inner, wait_gate, ..
        } = blocked_inner();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            drop(ShutdownOnDrop {
                inner: Some(inner),
                deadline: Instant::now() + Duration::from_millis(50),
            });
            let _ = tx.send(started.elapsed());
        });

        // The property is causal, not a stopwatch reading: Drop must return
        // WITHOUT waiting for the child, so it must report back before the
        // gate is released. A blocked Drop cannot, whatever the machine load.
        //
        // Asserting a wall-clock bound instead conflated that with "finished
        // inside 100ms", which a loaded runner broke by 0.2ms. The window
        // below is generous because it only bounds how long a *failure* takes
        // to detect: a correct Drop returns at its 50ms deadline and never
        // approaches it.
        let timely = rx.recv_timeout(Duration::from_secs(5));
        release_wait(&wait_gate);
        let returned_before_release = timely.is_ok();
        let elapsed = timely
            .or_else(|_| rx.recv_timeout(Duration::from_secs(5)))
            .expect("shutdown did not unblock even after releasing fake child");
        worker.join().expect("shutdown worker panicked");
        assert!(
            returned_before_release,
            "Drop blocked in child.wait() until the fake child was released              (took {elapsed:?}); its deadline should have bounded it"
        );
    }

    #[test]
    fn shutdown_does_not_hold_child_mutex_while_reaping() {
        let BlockedFixture {
            mut inner,
            child,
            wait_gate,
            waits,
            ..
        } = blocked_inner();
        let worker = thread::spawn(move || {
            inner.shutdown_with_deadline(Instant::now() + Duration::from_millis(50));
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while waits.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(waits.load(Ordering::SeqCst), 1, "fake wait never started");

        let child_mutex_available = child.try_lock().is_ok();
        release_wait(&wait_gate);
        worker.join().expect("shutdown worker panicked");
        assert!(
            child_mutex_available,
            "shutdown held the child mutex across reaping"
        );
    }

    struct ReadyChild {
        polls: Arc<AtomicUsize>,
        waits: Arc<AtomicUsize>,
    }

    impl UnixChild for ReadyChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            Ok(std::process::ExitStatus::from_raw(0))
        }

        fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(std::process::ExitStatus::from_raw(0)))
        }
    }

    #[test]
    fn shutdown_reaps_ready_child_exactly_once() {
        let polls = Arc::new(AtomicUsize::new(0));
        let waits = Arc::new(AtomicUsize::new(0));
        let child: Arc<Mutex<Option<Box<dyn UnixChild>>>> =
            Arc::new(Mutex::new(Some(Box::new(ReadyChild {
                polls: Arc::clone(&polls),
                waits: Arc::clone(&waits),
            }))));
        let mut inner = SpawnedInner {
            child,
            pgid: i32::MAX,
        };

        inner.shutdown_with_deadline(Instant::now() + Duration::from_secs(1));

        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(waits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shutdown_falls_back_to_direct_kill_when_group_signal_fails() {
        let BlockedFixture {
            mut inner,
            wait_gate,
            kills,
            ..
        } = blocked_inner();
        release_wait(&wait_gate);

        inner.shutdown_with_deadline(Instant::now() + Duration::from_secs(1));

        assert_eq!(kills.load(Ordering::SeqCst), 1);
    }
}
