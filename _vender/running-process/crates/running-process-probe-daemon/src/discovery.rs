//! The owner-only discovery file.
//!
//! A client that knows nothing but the beacon port reads this file to learn
//! the control socket, HTTP port, and bearer token.
//!
//! The token is the reason the permissions matter. It is the daemon's
//! authentication secret, so the file must be readable only by its owner —
//! `0o600` on Unix, a protected DACL on Windows. Directory hardening reuses
//! `running_process::broker::secure_dir` rather than reimplementing the
//! Windows SDDL, which has already been audited and fixed once upstream.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Contents of `rpprobed.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryInfo {
    /// Always 1 for this format. Lets a future reader reject rather than
    /// misparse.
    pub wire_version: u32,
    /// Platform path/name of the daemon's control socket.
    pub control_socket: String,
    /// Loopback port of the daemon's HTTP listener.
    pub http_port: u16,
    /// 64 lowercase hex chars (32 bytes of entropy).
    pub bearer_token: String,
    /// PID of the daemon that published this file.
    pub daemon_pid: u32,
}

/// Name of the discovery file inside the runtime directory.
pub const DISCOVERY_FILE: &str = "rpprobed.json";

/// Per-user runtime directory holding the discovery file and Unix socket.
///
/// `override_dir` (from `--runtime-dir`) wins, which is what lets tests run
/// concurrently without colliding on a shared machine-wide path.
#[allow(unsafe_code)] // getuid: FFI with no safe wrapper; see names.rs
pub fn discovery_dir(override_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.to_path_buf();
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("running-process").join("probe")
    }
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        tmp.join(format!(".rp-{uid}-probe"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(dir).join("running-process").join("probe")
        } else {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/running-process-{uid}/probe"))
        }
    }
}

/// Generate the bearer token: 32 bytes of OS entropy as lowercase hex.
pub fn generate_bearer_token() -> io::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(format!("getrandom: {e}")))?;
    let mut hex = String::with_capacity(64);
    for b in raw {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// Publish the discovery file into `dir`, hardening the directory first.
///
/// Written to a temporary name in the same directory and `rename`d into place.
/// A reader therefore sees either no file or a complete one — never a
/// half-written record it might parse as a valid endpoint.
pub fn write_discovery_file(dir: &Path, info: &DiscoveryInfo) -> io::Result<PathBuf> {
    running_process::broker::secure_dir::ensure_private_dir(dir)?;

    let final_path = dir.join(DISCOVERY_FILE);
    let tmp_path = dir.join(format!("{DISCOVERY_FILE}.{}.tmp", std::process::id()));

    let body = serde_json::to_vec_pretty(info)
        .map_err(|e| io::Error::other(format!("serialize discovery info: {e}")))?;
    std::fs::write(&tmp_path, &body)?;

    // Tighten the file itself before publishing. On Windows it inherits the
    // directory's protected DACL; on Unix the default mode is too permissive
    // for a file holding a bearer token.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Read a published discovery file.
pub fn read_discovery_file(dir: &Path) -> io::Result<DiscoveryInfo> {
    let bytes = std::fs::read(dir.join(DISCOVERY_FILE))?;
    let info: DiscoveryInfo = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("parse discovery info: {e}")))?;
    if info.wire_version != 1 {
        return Err(io::Error::other(format!(
            "unsupported discovery wire_version {}",
            info.wire_version
        )));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DiscoveryInfo {
        DiscoveryInfo {
            wire_version: 1,
            control_socket: "/tmp/x.sock".into(),
            http_port: 54123,
            bearer_token: "ab".repeat(32),
            daemon_pid: 4242,
        }
    }

    #[test]
    fn discovery_file_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        write_discovery_file(dir.path(), &sample()).unwrap();
        assert_eq!(read_discovery_file(dir.path()).unwrap(), sample());
    }

    #[test]
    fn unsupported_wire_version_is_refused_not_misparsed() {
        let dir = tempfile::tempdir().unwrap();
        let mut info = sample();
        info.wire_version = 99;
        write_discovery_file(dir.path(), &info).unwrap();
        assert!(read_discovery_file(dir.path()).is_err());
    }

    /// The token is the daemon's auth secret; group/other must not read it.
    #[cfg(unix)]
    #[test]
    fn discovery_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = write_discovery_file(dir.path(), &sample()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o} exposes the bearer token");
    }

    #[test]
    fn bearer_token_is_64_hex_chars_and_not_constant() {
        let a = generate_bearer_token().unwrap();
        let b = generate_bearer_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_ne!(a, b, "tokens must not repeat across calls");
    }

    #[test]
    fn runtime_dir_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(discovery_dir(Some(dir.path())), dir.path());
    }

    /// No partially-written file is ever visible under the published name.
    #[test]
    fn publish_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        write_discovery_file(dir.path(), &sample()).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }
}
