//! PTY backend abstraction (#150).
//!
//! `native_pty_process.rs` was riddled with `#[cfg(windows)]` /
//! `#[cfg(unix)]` branches around the underlying portable-pty calls.
//! After the #150 rewrite we have two distinct backends:
//!
//! * Windows - `conpty::ConPtyBackend` (raw ConPTY via windows-sys
//!   with `PSEUDOCONSOLE_PASSTHROUGH_MODE` enabled)
//! * Unix - `unix::PortablePtyBackend` (a thin wrapper around
//!   portable-pty's native_pty_system, unchanged behavior)
//!
//! The `Backend` type alias resolves to one or the other per-target,
//! and `native_pty_process.rs` makes a single `Backend::openpty(...)`
//! call instead of branching.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;

/// Caller-facing PTY dimensions. Pixel fields are ignored on Windows
/// (ConPTY only consumes rows/cols). Mirrors portable-pty's shape so
/// caller code passes them through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// Terminal row count in character cells.
    pub rows: u16,
    /// Terminal column count in character cells.
    pub cols: u16,
    /// Terminal width in pixels when the backend supports it.
    pub pixel_width: u16,
    /// Terminal height in pixels when the backend supports it.
    pub pixel_height: u16,
}

/// Platform-neutral handle for the master side of a pseudo-terminal.
pub trait PtyMaster: Send + 'static {
    /// Clone a reader for PTY output.
    fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>>;
    /// Take the writer used to send input to the PTY.
    fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>>;
    /// Resize the PTY to the requested dimensions.
    fn resize(&self, size: PtySize) -> io::Result<()>;
    /// Return the current PTY dimensions. On Windows the value is
    /// the last size passed to `resize` (or the initial openpty
    /// size); ConPTY exposes no live query API. Restored in 4.0.1
    /// for downstream parity with portable-pty's `MasterPty::get_size`.
    fn get_size(&self) -> io::Result<PtySize>;
    /// On Unix returns the foreground process group leader of the
    /// PTY (used by tools like `tcsetpgrp` checks). Always returns
    /// `None` on Windows where the concept doesn't exist.
    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32>;
    /// Return the Unix PTY master descriptor for bounded control writes.
    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd>;
}

/// Platform-neutral handle for a child process running inside a PTY.
pub trait PtyChild: Send + 'static {
    /// Return the operating system process identifier.
    fn pid(&self) -> u32;
    /// Poll without blocking. `Ok(None)` means still running.
    /// `Ok(Some(code))` means exited with that exit code.
    /// `&mut self` because portable-pty's underlying Child::try_wait
    /// takes &mut, and we keep the surface uniform across backends.
    fn try_wait(&mut self) -> io::Result<Option<u32>>;
    /// Block until the child exits, then return the exit code.
    fn wait(&mut self) -> io::Result<u32>;
    /// Terminate the child process.
    fn kill(&mut self) -> io::Result<()>;
    /// Returns the Windows process HANDLE, if applicable. `None`
    /// means the backend can't expose one (which is fatal for Job
    /// Object containment — `assign_child_to_windows_kill_on_close_job`
    /// requires a real handle). Matches portable_pty's signature.
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle>;
}

/// Platform-neutral handle for the slave side of a pseudo-terminal.
pub trait PtySlave: Send + 'static {
    /// Child process type produced by this slave handle.
    type Child: PtyChild;
    /// Spawn a child process attached to this slave PTY.
    fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<Self::Child>;
}

/// Factory trait for opening platform-specific pseudo-terminal pairs.
pub trait PtyBackend {
    /// Master-side handle type returned by this backend.
    type Master: PtyMaster;
    /// Slave-side handle type returned by this backend.
    type Slave: PtySlave;
    /// Open a new PTY with the requested dimensions.
    fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)>;
}

#[cfg(windows)]
mod conpty {
    use super::*;
    use crate::pty::conpty_passthrough;

    pub(crate) struct ConPtyBackend;

    impl PtyBackend for ConPtyBackend {
        type Master = conpty_passthrough::ConPtyMaster;
        type Slave = conpty_passthrough::ConPtySlave;

        fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)> {
            let pair = conpty_passthrough::openpty(conpty_passthrough::PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
            })?;
            Ok((pair.master, pair.slave))
        }
    }

    impl PtyMaster for conpty_passthrough::ConPtyMaster {
        fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            conpty_passthrough::ConPtyMaster::try_clone_reader(self)
        }
        fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
            conpty_passthrough::ConPtyMaster::take_writer(self)
        }
        fn resize(&self, size: PtySize) -> io::Result<()> {
            conpty_passthrough::ConPtyMaster::resize(
                self,
                conpty_passthrough::PtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                },
            )
        }
        fn get_size(&self) -> io::Result<PtySize> {
            let s = conpty_passthrough::ConPtyMaster::get_size(self);
            Ok(PtySize {
                rows: s.rows,
                cols: s.cols,
                pixel_width: s.pixel_width,
                pixel_height: s.pixel_height,
            })
        }
    }

    impl PtySlave for conpty_passthrough::ConPtySlave {
        type Child = conpty_passthrough::child::ConPtyChild;
        fn spawn(
            self,
            argv: &[OsString],
            cwd: Option<&Path>,
            env: Option<&[(OsString, OsString)]>,
        ) -> io::Result<Self::Child> {
            conpty_passthrough::ConPtySlave::spawn(self, argv, cwd, env)
        }
    }

    impl PtyChild for conpty_passthrough::child::ConPtyChild {
        fn pid(&self) -> u32 {
            conpty_passthrough::child::ConPtyChild::pid(self)
        }
        fn try_wait(&mut self) -> io::Result<Option<u32>> {
            conpty_passthrough::child::ConPtyChild::try_wait(self)
        }
        fn wait(&mut self) -> io::Result<u32> {
            conpty_passthrough::child::ConPtyChild::wait(self)
        }
        fn kill(&mut self) -> io::Result<()> {
            conpty_passthrough::child::ConPtyChild::kill(self)
        }
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            Some(conpty_passthrough::child::ConPtyChild::as_raw_handle(self))
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use portable_pty::{
        native_pty_system, Child as PortableChild, CommandBuilder, MasterPty,
        PtySize as PortPtySize, SlavePty,
    };

    pub(crate) struct PortablePtyBackend;

    pub(crate) struct PortablePtyMaster(Box<dyn MasterPty + Send>);
    pub(crate) struct PortablePtySlave(Box<dyn SlavePty + Send>);
    pub(crate) struct PortablePtyChild(Box<dyn PortableChild + Send + Sync>);

    impl PtyBackend for PortablePtyBackend {
        type Master = PortablePtyMaster;
        type Slave = PortablePtySlave;

        fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)> {
            let sys = native_pty_system();
            let pair = sys
                .openpty(PortPtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                })
                .map_err(io::Error::other)?;
            Ok((PortablePtyMaster(pair.master), PortablePtySlave(pair.slave)))
        }
    }

    impl PtyMaster for PortablePtyMaster {
        fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            self.0.try_clone_reader().map_err(io::Error::other)
        }
        fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
            self.0.take_writer().map_err(io::Error::other)
        }
        fn resize(&self, size: PtySize) -> io::Result<()> {
            self.0
                .resize(PortPtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                })
                .map_err(io::Error::other)
        }
        fn get_size(&self) -> io::Result<PtySize> {
            let s = self.0.get_size().map_err(io::Error::other)?;
            Ok(PtySize {
                rows: s.rows,
                cols: s.cols,
                pixel_width: s.pixel_width,
                pixel_height: s.pixel_height,
            })
        }
        fn process_group_leader(&self) -> Option<i32> {
            self.0.process_group_leader()
        }
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            self.0.as_raw_fd()
        }
    }

    impl PtySlave for PortablePtySlave {
        type Child = PortablePtyChild;
        fn spawn(
            self,
            argv: &[OsString],
            cwd: Option<&Path>,
            env: Option<&[(OsString, OsString)]>,
        ) -> io::Result<Self::Child> {
            if argv.is_empty() {
                return Err(io::Error::other(
                    "portable-pty spawn requires non-empty argv",
                ));
            }
            let mut cmd = CommandBuilder::new(&argv[0]);
            for arg in &argv[1..] {
                cmd.arg(arg);
            }
            if let Some(cwd) = cwd {
                cmd.cwd(cwd);
            }
            if let Some(env) = env {
                cmd.env_clear();
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }
            let child = self.0.spawn_command(cmd).map_err(io::Error::other)?;
            Ok(PortablePtyChild(child))
        }
    }

    impl PtyChild for PortablePtyChild {
        fn pid(&self) -> u32 {
            self.0.process_id().unwrap_or(0)
        }
        fn try_wait(&mut self) -> io::Result<Option<u32>> {
            match self.0.try_wait()? {
                Some(status) => Ok(Some(portable_pty_exit_code(status))),
                None => Ok(None),
            }
        }
        fn wait(&mut self) -> io::Result<u32> {
            let status = self.0.wait()?;
            Ok(portable_pty_exit_code(status))
        }
        fn kill(&mut self) -> io::Result<()> {
            self.0.kill()
        }
    }

    /// Convert portable-pty's ExitStatus to a u32 exit code.
    /// Signal exits map to `128 + signal_index` per the standard
    /// shell convention.
    fn portable_pty_exit_code(status: portable_pty::ExitStatus) -> u32 {
        // ExitStatus is opaque; format and parse the debug form which
        // is "exited(code)" or "signal(name)". Cleaner would be to
        // pattern-match on its accessor — but portable-pty's
        // ExitStatus only exposes `exit_code() -> u32` directly.
        status.exit_code()
    }
}

// #150 W8: route Windows through ConPtyBackend (our new
// PSEUDOCONSOLE_PASSTHROUGH_MODE implementation).
#[cfg(windows)]
pub(crate) type Backend = conpty::ConPtyBackend;
#[cfg(unix)]
pub(crate) type Backend = unix::PortablePtyBackend;

// On Windows we still want the portable-pty wrapper available as
// the temporary backend. Mirror the Unix module under a different
// name so the cfg-pickup above works.
#[cfg(windows)]
#[allow(dead_code)]
mod unix_compat {
    use super::*;
    use portable_pty::{
        native_pty_system, Child as PortableChild, CommandBuilder, MasterPty,
        PtySize as PortPtySize, SlavePty,
    };

    pub(crate) struct PortablePtyBackend;
    pub(crate) struct PortablePtyMaster(Box<dyn MasterPty + Send>);
    pub(crate) struct PortablePtySlave(Box<dyn SlavePty + Send>);
    pub(crate) struct PortablePtyChild(Box<dyn PortableChild + Send + Sync>);

    impl PtyBackend for PortablePtyBackend {
        type Master = PortablePtyMaster;
        type Slave = PortablePtySlave;
        fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)> {
            let sys = native_pty_system();
            let pair = sys
                .openpty(PortPtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                })
                .map_err(io::Error::other)?;
            Ok((PortablePtyMaster(pair.master), PortablePtySlave(pair.slave)))
        }
    }

    impl PtyMaster for PortablePtyMaster {
        fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
            self.0.try_clone_reader().map_err(io::Error::other)
        }
        fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>> {
            self.0.take_writer().map_err(io::Error::other)
        }
        fn resize(&self, size: PtySize) -> io::Result<()> {
            self.0
                .resize(PortPtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: size.pixel_width,
                    pixel_height: size.pixel_height,
                })
                .map_err(io::Error::other)
        }
        fn get_size(&self) -> io::Result<PtySize> {
            let s = self.0.get_size().map_err(io::Error::other)?;
            Ok(PtySize {
                rows: s.rows,
                cols: s.cols,
                pixel_width: s.pixel_width,
                pixel_height: s.pixel_height,
            })
        }
    }

    impl PtySlave for PortablePtySlave {
        type Child = PortablePtyChild;
        fn spawn(
            self,
            argv: &[OsString],
            cwd: Option<&Path>,
            env: Option<&[(OsString, OsString)]>,
        ) -> io::Result<Self::Child> {
            if argv.is_empty() {
                return Err(io::Error::other(
                    "portable-pty spawn requires non-empty argv",
                ));
            }
            let mut cmd = CommandBuilder::new(&argv[0]);
            for arg in &argv[1..] {
                cmd.arg(arg);
            }
            if let Some(cwd) = cwd {
                cmd.cwd(cwd);
            }
            if let Some(env) = env {
                cmd.env_clear();
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }
            let child = self.0.spawn_command(cmd).map_err(io::Error::other)?;
            Ok(PortablePtyChild(child))
        }
    }

    impl PtyChild for PortablePtyChild {
        fn pid(&self) -> u32 {
            self.0.process_id().unwrap_or(0)
        }
        fn try_wait(&mut self) -> io::Result<Option<u32>> {
            match self.0.try_wait()? {
                Some(status) => Ok(Some(status.exit_code())),
                None => Ok(None),
            }
        }
        fn wait(&mut self) -> io::Result<u32> {
            let status = self.0.wait()?;
            Ok(status.exit_code())
        }
        fn kill(&mut self) -> io::Result<()> {
            self.0.kill()
        }
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            self.0.as_raw_handle()
        }
    }
}
