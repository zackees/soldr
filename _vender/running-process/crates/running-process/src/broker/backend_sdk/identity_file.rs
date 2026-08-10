//! JSON identity-sidecar persistence for direct-daemon consumers (#412).
//!
//! The lightweight alternative to a full
//! [`CacheManifest`](crate::broker::protocol::CacheManifest): a daemon
//! persists its [`DaemonProcess`] identity next to its PID file at
//! startup, and clients later probe it with
//! [`BackendHandle::probe_with_service`](crate::broker::backend_handle::BackendHandle::probe_with_service).
//! soldr's `daemon-identity.json` sidecar pioneered the pattern; this
//! module owns it so consumers stop re-implementing tolerant reads and
//! atomic writes.
//!
//! Contract:
//!
//! - [`write_daemon_identity_file`] writes atomically (temp file +
//!   rename) so probers never observe a torn sidecar.
//! - [`read_daemon_identity_file`] is tolerant: absent or malformed
//!   files return `None`, and the caller degrades to its fallback probe
//!   (typically a PID file).
//! - [`try_read_daemon_identity_file`] preserves errors for
//!   diagnostics/doctor surfaces that report malformed sidecars
//!   separately from a normal miss.
//! - [`remove_daemon_identity_file`] is best-effort, for clean
//!   shutdown.

use std::io;
use std::path::Path;

use crate::broker::backend_lifecycle::identity::DaemonProcess;

/// Persist `daemon` as pretty JSON at `path`, atomically.
///
/// The parent directory must exist (daemons create it before writing
/// their PID file). The write goes to `path` with a `.tmp` suffix first
/// and is renamed into place.
pub fn write_daemon_identity_file(path: &Path, daemon: &DaemonProcess) -> io::Result<()> {
    let json = serde_json::to_string_pretty(daemon)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, json)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

/// Read a persisted daemon identity, tolerantly.
///
/// Absent or malformed files return `None` — the caller degrades to its
/// fallback probe. Use [`try_read_daemon_identity_file`] where
/// malformed sidecars must be reported.
pub fn read_daemon_identity_file(path: &Path) -> Option<DaemonProcess> {
    try_read_daemon_identity_file(path).ok().flatten()
}

/// Fallible variant of [`read_daemon_identity_file`].
///
/// Returns `Ok(None)` when the file does not exist, `Err` when it
/// exists but cannot be read or parsed.
pub fn try_read_daemon_identity_file(path: &Path) -> io::Result<Option<DaemonProcess>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let daemon = serde_json::from_str(&raw)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(daemon))
}

/// Remove the identity sidecar, best-effort. Call on clean shutdown.
pub fn remove_daemon_identity_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::protocol::Endpoint;

    fn test_daemon() -> DaemonProcess {
        let endpoint =
            Endpoint::unix_socket("sidecar-test", "/tmp/sidecar-test.sock").expect("endpoint");
        DaemonProcess::current_process(endpoint, Some(30)).expect("identity")
    }

    #[test]
    fn identity_round_trips_through_sidecar() {
        let dir = std::env::temp_dir().join(format!(
            "rp-identity-sidecar-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("daemon-identity.json");

        let daemon = test_daemon();
        write_daemon_identity_file(&path, &daemon).expect("write");
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(read_daemon_identity_file(&path), Some(daemon.clone()));
        assert_eq!(
            try_read_daemon_identity_file(&path).expect("try read"),
            Some(daemon)
        );

        remove_daemon_identity_file(&path);
        assert_eq!(read_daemon_identity_file(&path), None);
        assert_eq!(try_read_daemon_identity_file(&path).expect("absent"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_sidecar_is_tolerated_by_read_and_reported_by_try_read() {
        let dir = std::env::temp_dir().join(format!(
            "rp-identity-sidecar-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("daemon-identity.json");
        std::fs::write(&path, "not json").expect("write garbage");

        assert_eq!(read_daemon_identity_file(&path), None);
        assert!(try_read_daemon_identity_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
