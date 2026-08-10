// Unix-specific daemon operations (double-fork, setsid, redirect to /dev/null)

/// Daemonize the current process using the classic double-fork technique.
///
/// After this call returns `Ok(())`, the calling code is running in a fully
/// detached daemon process (the grandchild). The original process and the
/// intermediate child have both exited.
#[cfg(unix)]
pub fn daemonize() -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::CString;

    // --- First fork ----------------------------------------------------------
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("first fork failed: {}", std::io::Error::last_os_error()).into());
    }
    if pid > 0 {
        // Parent – exit immediately so the shell returns.
        unsafe { libc::_exit(0) };
    }

    // --- Child: become session leader ----------------------------------------
    if unsafe { libc::setsid() } < 0 {
        return Err(format!("setsid failed: {}", std::io::Error::last_os_error()).into());
    }

    // --- Second fork (prevents re-acquiring a controlling terminal) ----------
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("second fork failed: {}", std::io::Error::last_os_error()).into());
    }
    if pid > 0 {
        // First child exits.
        unsafe { libc::_exit(0) };
    }

    // --- Grandchild: the actual daemon process -------------------------------

    // Set umask
    unsafe { libc::umask(0o027) };

    // Redirect stdin / stdout / stderr to /dev/null
    let devnull_path = CString::new("/dev/null").unwrap();
    let devnull_fd = unsafe { libc::open(devnull_path.as_ptr(), libc::O_RDWR) };
    if devnull_fd < 0 {
        return Err(format!(
            "failed to open /dev/null: {}",
            std::io::Error::last_os_error()
        )
        .into());
    }

    // dup2 onto stdin(0), stdout(1), stderr(2)
    for fd in 0..=2 {
        if unsafe { libc::dup2(devnull_fd, fd) } < 0 {
            return Err(format!(
                "dup2 to fd {} failed: {}",
                fd,
                std::io::Error::last_os_error()
            )
            .into());
        }
    }

    // Close the original /dev/null fd if it isn't one of 0-2
    if devnull_fd > 2 {
        unsafe { libc::close(devnull_fd) };
    }

    Ok(())
}
