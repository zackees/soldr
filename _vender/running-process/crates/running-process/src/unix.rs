#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSignal {
    Interrupt,
    Terminate,
    Kill,
}

#[cfg(unix)]
pub fn unix_set_priority(pid: u32, nice: i32) -> Result<(), std::io::Error> {
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub fn unix_signal_process(pid: u32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let result = unsafe { libc::kill(pid as i32, unix_signal_raw(signal)) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub fn unix_signal_process_group(pid: i32, signal: UnixSignal) -> Result<(), std::io::Error> {
    let result = unsafe { libc::killpg(pid, unix_signal_raw(signal)) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn unix_signal_raw(signal: UnixSignal) -> i32 {
    match signal {
        UnixSignal::Interrupt => libc::SIGINT,
        UnixSignal::Terminate => libc::SIGTERM,
        UnixSignal::Kill => libc::SIGKILL,
    }
}
