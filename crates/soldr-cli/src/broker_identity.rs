//! Authoritative Soldr broker installation and endpoint identity (#2476).
//!
//! Broker identity is derived only from the home-anchored installed broker
//! executable. Cache roots, runtime roots, package versions, daemon routes,
//! protocol generations, and caller namespaces never participate. The
//! logical identity and physical bind location are deliberately separate:
//! Unix may bind at a deterministic machine-local fallback when the home path
//! cannot host a socket, while diagnostics continue to display both values.

use running_process::broker::protocol::Endpoint;
use std::path::{Path, PathBuf};

/// Maximum complete Windows named-pipe path length from the #2476 contract.
const WINDOWS_PIPE_NAME_LIMIT: usize = 256;
const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Why the physical endpoint differs from the canonical logical socket path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerEndpointFallback {
    /// The Unix logical path, including its trailing NUL, exceeds `sun_path`.
    UnixSunPathOverflow,
    /// The home-anchored broker directory is on a filesystem that cannot
    /// safely host this singleton socket.
    UnixNonBindableFilesystem,
    /// The injective Windows leaf would exceed the complete pipe-name limit.
    WindowsPipeOverflow,
}

/// One authoritative broker resolution shared by bind, dial, admin, and
/// resurrection code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedBrokerEndpoint {
    /// Exact home-anchored executable staged and launched by front doors.
    pub executable_path: PathBuf,
    /// Canonical logical socket identity derived from `executable_path`.
    pub logical_socket_path: String,
    /// Complete platform endpoint used for bind and connect.
    pub bind_endpoint: String,
    /// Bare Windows pipe leaf for APIs that reject `\\.\pipe\`; absent on Unix.
    pub windows_pipe_leaf: Option<String>,
    /// The pre-fallback percent-encoded Windows leaf, when it overflowed.
    pub oversized_windows_pipe_leaf: Option<String>,
    /// Deterministic bind-location fallback, if selected.
    pub fallback: Option<BrokerEndpointFallback>,
    /// Machine-local fenced-resurrection lease database.
    pub lease_database_path: PathBuf,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct DoctorBrokerEndpoint {
    pub(crate) executable_path: Option<String>,
    pub(crate) logical_socket_path: Option<String>,
    pub(crate) bind_endpoint: Option<String>,
    pub(crate) fallback: Option<String>,
    pub(crate) resolution_error: Option<String>,
}

pub(crate) fn doctor_endpoint() -> DoctorBrokerEndpoint {
    match ResolvedBrokerEndpoint::resolve() {
        Ok(endpoint) => DoctorBrokerEndpoint {
            executable_path: Some(endpoint.executable_path.display().to_string()),
            logical_socket_path: Some(endpoint.logical_socket_path),
            bind_endpoint: Some(endpoint.bind_endpoint),
            fallback: endpoint.fallback.map(|value| format!("{value:?}")),
            resolution_error: None,
        },
        Err(error) => DoctorBrokerEndpoint {
            executable_path: None,
            logical_socket_path: None,
            bind_endpoint: None,
            fallback: None,
            resolution_error: Some(error.to_string()),
        },
    }
}

pub(crate) fn print_doctor_endpoint() {
    let endpoint = doctor_endpoint();
    println!("\nbroker endpoint:");
    if let Some(error) = endpoint.resolution_error {
        println!("  resolution error: {error}");
        return;
    }
    println!(
        "  executable: {}",
        endpoint.executable_path.as_deref().unwrap_or("unavailable")
    );
    println!(
        "  logical:    {}",
        endpoint
            .logical_socket_path
            .as_deref()
            .unwrap_or("unavailable")
    );
    println!(
        "  bind:       {}",
        endpoint.bind_endpoint.as_deref().unwrap_or("unavailable")
    );
    if let Some(fallback) = endpoint.fallback {
        println!("  fallback:   {fallback}");
    }
}

impl ResolvedBrokerEndpoint {
    /// Resolve the running user's one Soldr broker installation.
    pub fn resolve() -> Result<Self, BrokerIdentityError> {
        let home = crate::core::user_home_dir()
            .map_err(|error| BrokerIdentityError::Home(error.to_string()))?;
        if !home.is_absolute() {
            return Err(BrokerIdentityError::RelativeHome(home));
        }

        #[cfg(windows)]
        {
            let executable = authoritative_broker_executable(&home, "soldr-broker.exe");
            let local_app_data = std::env::var_os("LOCALAPPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData").join("Local"));
            resolve_windows_for_executable(&executable, &local_app_data)
        }

        #[cfg(unix)]
        {
            let executable = authoritative_broker_executable(&home, "soldr-broker");
            let runtime = unix_machine_runtime_dir();
            resolve_unix_for_executable(&executable, &runtime, None, unix_sun_path_capacity())
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = home;
            Err(BrokerIdentityError::UnsupportedPlatform)
        }
    }

    /// Create every broker-owned directory with owner-only permissions.
    /// Resolution itself remains read-only so compiler-wrapper dials do not
    /// mutate filesystem state.
    pub fn create_owner_only_directories(&self) -> Result<(), BrokerIdentityError> {
        let mut directories = Vec::new();
        if let Some(parent) = self.executable_path.parent() {
            directories.push(parent.to_path_buf());
        }
        #[cfg(unix)]
        if let Some(parent) = Path::new(&self.bind_endpoint).parent() {
            directories.push(parent.to_path_buf());
        }
        if let Some(parent) = self.lease_database_path.parent() {
            directories.push(parent.to_path_buf());
        }

        directories.sort();
        directories.dedup();
        for directory in directories {
            std::fs::create_dir_all(&directory).map_err(|source| {
                BrokerIdentityError::CreateDirectory {
                    path: directory.clone(),
                    source,
                }
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                    .map_err(|source| BrokerIdentityError::SecureDirectory {
                        path: directory.clone(),
                        source,
                    })?;
            }
        }
        Ok(())
    }

    /// Render the fallback decision with every value needed to diagnose it.
    pub fn fallback_diagnostic(&self) -> Option<String> {
        let fallback = self.fallback?;
        let mut message = format!(
            "broker endpoint fallback={fallback:?} executable={} logical={} bind={}",
            self.executable_path.display(),
            self.logical_socket_path,
            self.bind_endpoint
        );
        if let Some(oversized) = &self.oversized_windows_pipe_leaf {
            message.push_str(&format!(" oversized_leaf={oversized}"));
        }
        Some(message)
    }
}

/// The installed image is always derived from the expanded user home. The
/// detached-spawn path preserves the resolver's small environment input set,
/// so a broker launched from any other binary path cannot create an alternate
/// public endpoint merely because its executable leaf happens to match.
fn authoritative_broker_executable(home: &Path, installed_leaf: &str) -> PathBuf {
    canonicalize_existing_ancestor(&home.join(".soldr").join("broker").join(installed_leaf))
}

/// Canonicalize the deepest existing ancestor while preserving the not-yet-
/// created broker suffix. This makes a symlinked home spelling and its real
/// path converge before either the executable or socket exists.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let Some(leaf) = existing.file_name() else {
            return path.to_path_buf();
        };
        suffix.push(leaf.to_os_string());
        let Some(parent) = existing.parent() else {
            return path.to_path_buf();
        };
        existing = parent;
    }
    let Ok(mut canonical) = std::fs::canonicalize(existing) else {
        return path.to_path_buf();
    };
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    canonical
}

/// Endpoint derivation failures.
#[derive(Debug, thiserror::Error)]
pub enum BrokerIdentityError {
    /// No authoritative user home could be resolved.
    #[error("could not resolve the user home for the broker installation: {0}")]
    Home(String),
    /// A relative home would make endpoint identity depend on cwd.
    #[error("broker home must be absolute, got {0:?}")]
    RelativeHome(PathBuf),
    /// The authoritative executable path is not an absolute Windows path.
    #[error("Windows broker executable path must be absolute: {0}")]
    RelativeWindowsExecutable(String),
    /// A lexical parent component is never safe in canonical identity.
    #[error("Windows broker executable path contains unresolved '..': {0}")]
    WindowsParentComponent(String),
    /// The executable leaf does not end in `.exe`.
    #[error("Windows broker executable path must end in .exe: {0}")]
    MissingWindowsExeExtension(String),
    /// The Windows path prefix was malformed or unsupported.
    #[error("unsupported Windows broker executable path: {0}")]
    UnsupportedWindowsPath(String),
    /// An owner-only broker directory could not be created.
    #[error("could not create broker directory {path:?}: {source}")]
    CreateDirectory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Broker directory permissions could not be restricted.
    #[error("could not set owner-only permissions on {path:?}: {source}")]
    SecureDirectory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// Soldr does not support broker IPC on this platform.
    #[error("the stable Soldr broker endpoint is unsupported on this platform")]
    UnsupportedPlatform,
    /// A path-derived endpoint could not be represented by running-process.
    #[error("could not construct path-derived endpoint: {0}")]
    Endpoint(String),
}

#[cfg(unix)]
fn unix_sun_path_capacity() -> usize {
    if cfg!(target_os = "macos") {
        104
    } else {
        108
    }
}

#[cfg(unix)]
fn unix_machine_runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    #[cfg(target_os = "macos")]
    let base = base.join(format!("soldr-{}", unsafe { libc::getuid() }));

    #[cfg(not(target_os = "macos"))]
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/soldr-{}", unsafe { libc::getuid() })));

    base
}

/// Pure Unix resolver seam. `non_bindable_override` and `sun_path_capacity`
/// let Linux tests prove both deterministic fallback branches without a real
/// NFS mount or a second platform.
#[cfg(all(unix, test))]
fn resolve_unix_for_home(
    home: &Path,
    machine_runtime_dir: &Path,
    non_bindable_override: Option<bool>,
    sun_path_capacity: usize,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let executable = home.join(".soldr").join("broker").join("soldr-broker");
    resolve_unix_for_executable(
        &executable,
        machine_runtime_dir,
        non_bindable_override,
        sun_path_capacity,
    )
}

#[cfg(unix)]
fn resolve_unix_for_executable(
    executable: &Path,
    machine_runtime_dir: &Path,
    non_bindable_override: Option<bool>,
    sun_path_capacity: usize,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let executable = executable.to_path_buf();
    let logical = executable.with_file_name("soldr-broker.sock");
    let path_key = identity_key(unix_path_bytes(&logical));
    let lease_file = format!("broker-lease-{path_key}.sqlite3");
    let local_broker_dir = machine_runtime_dir.join("soldr").join("broker");
    let fallback_bind = local_broker_dir.join(format!("soldr-broker-{path_key}.sock"));
    let logical_len = unix_path_bytes(&logical).len().saturating_add(1);
    let non_bindable =
        non_bindable_override.unwrap_or_else(|| unix_path_is_on_non_bindable_filesystem(&logical));

    let (bind, fallback) = if logical_len > sun_path_capacity {
        (
            fallback_bind,
            Some(BrokerEndpointFallback::UnixSunPathOverflow),
        )
    } else if non_bindable {
        (
            fallback_bind,
            Some(BrokerEndpointFallback::UnixNonBindableFilesystem),
        )
    } else {
        (logical.clone(), None)
    };

    Ok(ResolvedBrokerEndpoint {
        executable_path: executable,
        logical_socket_path: logical.to_string_lossy().into_owned(),
        bind_endpoint: bind.to_string_lossy().into_owned(),
        windows_pipe_leaf: None,
        oversized_windows_pipe_leaf: None,
        fallback,
        lease_database_path: local_broker_dir.join(lease_file),
    })
}

#[cfg(unix)]
fn unix_path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str().as_bytes()
}

#[cfg(unix)]
fn unix_path_is_on_non_bindable_filesystem(path: &Path) -> bool {
    let existing = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| Path::new("/"));

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let Ok(c_path) = std::ffi::CString::new(existing.as_os_str().as_bytes()) else {
            return true;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        let magic = unsafe { stats.assume_init() }.f_type;
        // Network and userspace filesystems that commonly reject or fail to
        // coordinate Unix-domain socket binds. Values come from Linux magic.h.
        matches!(
            magic,
            0x0000_6969 // NFS
                | 0xFF53_4D42 // CIFS
                | 0xFE53_4D42 // SMB2
                | 0x7375_7245 // CODA
                | 0x5346_414F // AFS
                | 0x0000_564C // NCP
                | 0x00C3_6400 // CEPH
                | 0x0102_1997 // 9P
                | 0x6573_5546 // FUSE (sshfs and peers)
        )
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let Ok(c_path) = std::ffi::CString::new(existing.as_os_str().as_bytes()) else {
            return true;
        };
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return false;
        }
        let stats = unsafe { stats.assume_init() };
        let bytes: Vec<u8> = stats
            .f_fstypename
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .map(|byte| byte as u8)
            .collect();
        let fs = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        matches!(
            fs.as_str(),
            "nfs" | "smbfs" | "afpfs" | "webdav" | "osxfuse" | "macfuse"
        )
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = existing;
        false
    }
}

#[cfg(windows)]
fn resolve_windows_for_executable(
    executable: &Path,
    local_app_data: &Path,
) -> Result<ResolvedBrokerEndpoint, BrokerIdentityError> {
    let pipe = windows_pipe_from_executable_with_suffix(
        &executable.to_string_lossy(),
        ".sock",
        "soldr-broker",
    )?;
    let lease_file = format!(
        "broker-lease-{}.sqlite3",
        identity_key(pipe.logical_socket_path.as_bytes())
    );
    Ok(ResolvedBrokerEndpoint {
        executable_path: executable.to_path_buf(),
        logical_socket_path: pipe.logical_socket_path,
        bind_endpoint: format!("{WINDOWS_PIPE_PREFIX}{}", pipe.pipe_leaf),
        windows_pipe_leaf: Some(pipe.pipe_leaf),
        oversized_windows_pipe_leaf: pipe.oversized_leaf,
        fallback: pipe
            .overflowed
            .then_some(BrokerEndpointFallback::WindowsPipeOverflow),
        lease_database_path: local_app_data.join("soldr").join("broker").join(lease_file),
    })
}

fn identity_key(identity: &[u8]) -> String {
    let digest = blake3::hash(identity);
    digest.to_hex()[..16].to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WindowsBrokerPipe {
    logical_socket_path: String,
    pipe_leaf: String,
    oversized_leaf: Option<String>,
    overflowed: bool,
}

/// Canonicalize and encode a Windows broker executable path without relying
/// on host-platform `Path` parsing, so the complete Windows matrix runs under
/// the Linux development harness too.
fn windows_broker_pipe_from_executable(
    executable: &str,
) -> Result<WindowsBrokerPipe, BrokerIdentityError> {
    windows_pipe_from_executable_with_suffix(executable, ".sock", "soldr-broker")
}

fn windows_pipe_from_executable_with_suffix(
    executable: &str,
    socket_suffix: &str,
    overflow_prefix: &str,
) -> Result<WindowsBrokerPipe, BrokerIdentityError> {
    let original = executable.to_string();
    let mut normalized = executable.replace('/', "\\");

    if ascii_starts_with_ignore_case(&normalized, r"\\?\UNC\") {
        normalized = format!(r"\\{}", &normalized[8..]);
    } else if ascii_starts_with_ignore_case(&normalized, r"\\?\") {
        normalized = normalized[4..].to_string();
    }

    let (root, remainder, minimum_components) = if normalized.len() >= 3
        && normalized.as_bytes()[0].is_ascii_alphabetic()
        && normalized.as_bytes()[1] == b':'
        && normalized.as_bytes()[2] == b'\\'
    {
        (normalized[..3].to_string(), &normalized[3..], 0_usize)
    } else if let Some(remainder) = normalized.strip_prefix(r"\\") {
        (r"\\".to_string(), remainder, 2_usize)
    } else {
        return Err(BrokerIdentityError::RelativeWindowsExecutable(original));
    };

    let mut components = Vec::new();
    for component in remainder.split('\\') {
        match component {
            "" | "." => {}
            ".." => return Err(BrokerIdentityError::WindowsParentComponent(original)),
            value => components.push(value.to_string()),
        }
    }
    if components.len() < minimum_components {
        return Err(BrokerIdentityError::UnsupportedWindowsPath(original));
    }
    let Some(last) = components.last_mut() else {
        return Err(BrokerIdentityError::MissingWindowsExeExtension(original));
    };
    if last.len() < 4 || !last[last.len() - 4..].eq_ignore_ascii_case(".exe") {
        return Err(BrokerIdentityError::MissingWindowsExeExtension(original));
    }
    last.truncate(last.len() - 4);
    last.push_str(socket_suffix);

    let joined = components.join("\\");
    let logical = if root == r"\\" {
        format!(r"\\{joined}")
    } else {
        format!("{root}{joined}")
    };
    let logical = ascii_lowercase(&logical);
    let encoded = percent_encode_pipe_leaf(logical.as_bytes());
    let overflowed = WINDOWS_PIPE_PREFIX.len() + encoded.len() > WINDOWS_PIPE_NAME_LIMIT;
    let (pipe_leaf, oversized_leaf) = if overflowed {
        let digest = blake3::hash(logical.as_bytes());
        let mut short = String::with_capacity(16);
        for byte in digest.as_bytes().iter().take(8) {
            use std::fmt::Write as _;
            let _ = write!(short, "{byte:02x}");
        }
        (format!("{overflow_prefix}-ovf-{short}"), Some(encoded))
    } else {
        (encoded, None)
    };

    Ok(WindowsBrokerPipe {
        logical_socket_path: logical,
        pipe_leaf,
        oversized_leaf,
        overflowed,
    })
}

/// Derive a private daemon SESSION endpoint solely from the canonical path of
/// the executable the broker will launch. No SID, separately queried user
/// identity, allocator index, or random token participates in the name.
pub fn daemon_session_endpoint_from_executable(
    executable: &Path,
) -> Result<Endpoint, BrokerIdentityError> {
    let executable = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    if !executable.is_absolute() {
        return Err(BrokerIdentityError::Endpoint(format!(
            "daemon executable path must be absolute: {}",
            executable.display()
        )));
    }
    let namespace_id = executable.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        let pipe = windows_pipe_from_executable_with_suffix(
            &executable.to_string_lossy(),
            ".session.sock",
            "soldr-daemon-session",
        )?;
        Endpoint::windows_pipe(namespace_id, pipe.pipe_leaf)
            .map_err(|error| BrokerIdentityError::Endpoint(error.to_string()))
    }
    #[cfg(unix)]
    {
        let executable_leaf = executable
            .file_name()
            .and_then(|leaf| leaf.to_str())
            .ok_or_else(|| {
                BrokerIdentityError::Endpoint(format!(
                    "daemon executable has no UTF-8 file name: {}",
                    executable.display()
                ))
            })?;
        let logical = executable.with_file_name(format!("{executable_leaf}.session.sock"));
        let bind = if unix_path_bytes(&logical).len() < unix_sun_path_capacity() {
            logical
        } else {
            let broker = ResolvedBrokerEndpoint::resolve()?;
            let broker_parent = Path::new(&broker.bind_endpoint)
                .parent()
                .unwrap_or_else(|| Path::new("/tmp"));
            let digest = blake3::hash(executable.to_string_lossy().as_bytes());
            let leaf = format!("d-{}.session.sock", &digest.to_hex()[..16]);
            let candidates = [
                broker_parent.join(&leaf),
                unix_machine_runtime_dir()
                    .join("soldr")
                    .join("daemon")
                    .join(&leaf),
                PathBuf::from(format!("/tmp/soldr-{}", unsafe { libc::getuid() }))
                    .join("daemon")
                    .join(&leaf),
            ];
            candidates
                .into_iter()
                .find(|path| unix_path_bytes(path).len() < unix_sun_path_capacity())
                .ok_or_else(|| {
                    BrokerIdentityError::Endpoint(format!(
                        "no Unix socket path can represent daemon executable {}",
                        executable.display()
                    ))
                })?
        };
        Endpoint::unix_socket(namespace_id, bind.to_string_lossy().into_owned())
            .map_err(|error| BrokerIdentityError::Endpoint(error.to_string()))
    }
}

fn ascii_starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ascii_lowercase(value: &str) -> String {
    let mut bytes = value.as_bytes().to_vec();
    bytes.make_ascii_lowercase();
    String::from_utf8(bytes).expect("ASCII folding preserves valid UTF-8")
}

fn percent_encode_pipe_leaf(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(linux_contract_mapping_is_exact, {
        #[cfg(unix)]
        {
            let endpoint = resolve_unix_for_home(
                Path::new("/home/niteris"),
                Path::new("/run/user/1000"),
                Some(false),
                108,
            )
            .expect("endpoint");
            assert_eq!(
                endpoint.executable_path,
                PathBuf::from("/home/niteris/.soldr/broker/soldr-broker")
            );
            assert_eq!(
                endpoint.logical_socket_path,
                "/home/niteris/.soldr/broker/soldr-broker.sock"
            );
            assert_eq!(endpoint.bind_endpoint, endpoint.logical_socket_path);
            assert_eq!(endpoint.fallback, None);
        }
    });

    crate::timed_test!(
        resurrection_leases_are_partitioned_by_broker_executable_path,
        {
            let a = format!(
                "broker-lease-{}.sqlite3",
                identity_key(b"/profiles/a/.soldr/broker/soldr-broker.sock")
            );
            let b = format!(
                "broker-lease-{}.sqlite3",
                identity_key(b"/profiles/b/.soldr/broker/soldr-broker.sock")
            );
            assert_ne!(a, b);
            assert!(a.starts_with("broker-lease-"));
            assert!(a.ends_with(".sqlite3"));
        }
    );

    crate::timed_test!(
        detached_broker_keeps_endpoint_beside_its_staged_executable,
        {
            #[cfg(unix)]
            {
                let endpoint = resolve_unix_for_executable(
                    Path::new("/mounted/home/.soldr/broker/soldr-broker"),
                    Path::new("/run/user/1000"),
                    Some(false),
                    108,
                )
                .expect("endpoint");
                assert_eq!(
                    endpoint.logical_socket_path,
                    "/mounted/home/.soldr/broker/soldr-broker.sock"
                );
                assert_eq!(endpoint.bind_endpoint, endpoint.logical_socket_path);
            }
        }
    );

    crate::timed_test!(windows_contract_mapping_is_exact, {
        let endpoint =
            windows_broker_pipe_from_executable(r"C:\Users\niteris\.soldr\broker\soldr-broker.exe")
                .expect("endpoint");
        assert_eq!(
            endpoint.logical_socket_path,
            r"c:\users\niteris\.soldr\broker\soldr-broker.sock"
        );
        assert_eq!(
            endpoint.pipe_leaf,
            r"c%3A%5Cusers%5Cniteris%5C.soldr%5Cbroker%5Csoldr-broker.sock"
        );
        assert!(!endpoint.overflowed);
    });

    crate::timed_test!(
        windows_daemon_session_mapping_is_exact_and_contains_no_sid,
        {
            let endpoint = windows_pipe_from_executable_with_suffix(
            r"C:\Users\niteris\.soldr\broker\routes\root-a\runtime\soldr-daemon\v0.9.0\soldr-daemon.exe",
            ".session.sock",
            "soldr-daemon-session",
        )
        .expect("endpoint");
            assert_eq!(
                endpoint.logical_socket_path,
                r"c:\users\niteris\.soldr\broker\routes\root-a\runtime\soldr-daemon\v0.9.0\soldr-daemon.session.sock"
            );
            assert_eq!(
                endpoint.pipe_leaf,
                r"c%3A%5Cusers%5Cniteris%5C.soldr%5Cbroker%5Croutes%5Croot-a%5Cruntime%5Csoldr-daemon%5Cv0.9.0%5Csoldr-daemon.session.sock"
            );
            assert!(!endpoint.pipe_leaf.contains("sid"));
        }
    );

    crate::timed_test!(unix_daemon_session_mapping_is_executable_sibling, {
        #[cfg(unix)]
        {
            let temp = tempfile::tempdir().expect("tempdir");
            let executable = temp.path().join("soldr-daemon");
            std::fs::write(&executable, b"daemon").expect("daemon image");
            let executable = std::fs::canonicalize(executable).expect("canonical daemon");
            let endpoint = daemon_session_endpoint_from_executable(&executable).expect("endpoint");
            assert_eq!(endpoint.namespace_id, executable.to_string_lossy());
            assert_eq!(
                endpoint.path,
                executable
                    .with_file_name("soldr-daemon.session.sock")
                    .to_string_lossy()
            );
            assert!(!endpoint.path.contains("sid"));
        }
    });

    crate::timed_test!(
        unix_daemon_session_mapping_distinguishes_executable_leaves,
        {
            #[cfg(unix)]
            {
                let temp = tempfile::tempdir().expect("tempdir");
                let first_executable = temp.path().join("soldr-daemon-a");
                let second_executable = temp.path().join("soldr-daemon-b");
                std::fs::write(&first_executable, b"a").expect("first image");
                std::fs::write(&second_executable, b"b").expect("second image");
                let first = daemon_session_endpoint_from_executable(&first_executable)
                    .expect("first endpoint");
                let second = daemon_session_endpoint_from_executable(&second_executable)
                    .expect("second endpoint");
                assert_ne!(first.path, second.path);
                assert!(first.path.ends_with("soldr-daemon-a.session.sock"));
                assert!(second.path.ends_with("soldr-daemon-b.session.sock"));
            }
        }
    );

    crate::timed_test!(
        canonical_existing_ancestor_collapses_symlinked_home_spelling,
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;

                let temp = tempfile::tempdir().expect("tempdir");
                let real = temp.path().join("real-home");
                let alias = temp.path().join("alias-home");
                std::fs::create_dir_all(&real).expect("real home");
                symlink(&real, &alias).expect("home symlink");
                let resolved = authoritative_broker_executable(&alias, "soldr-broker");
                assert_eq!(
                    resolved,
                    std::fs::canonicalize(&real)
                        .expect("canonical real home")
                        .join(".soldr/broker/soldr-broker")
                );
            }
        }
    );

    crate::timed_test!(
        unix_daemon_session_overflow_uses_a_short_path_derived_name,
        {
            #[cfg(unix)]
            {
                let temp = tempfile::tempdir().expect("tempdir");
                let directory = temp.path().join("long-route-segment".repeat(8));
                std::fs::create_dir_all(&directory).expect("long route directory");
                let executable = directory.join("soldr-daemon");
                std::fs::write(&executable, b"daemon").expect("daemon image");
                let executable = std::fs::canonicalize(executable).expect("canonical daemon");
                let first = daemon_session_endpoint_from_executable(&executable).expect("endpoint");
                let second =
                    daemon_session_endpoint_from_executable(&executable).expect("endpoint");
                assert_eq!(first, second);
                assert!(unix_path_bytes(Path::new(&first.path)).len() < unix_sun_path_capacity());
                assert!(first.path.ends_with(".session.sock"));
                assert!(!first.path.contains("sid"));
            }
        }
    );

    crate::timed_test!(windows_sanitizer_normalizes_supported_spellings, {
        let expected =
            windows_broker_pipe_from_executable(r"C:\Users\Me\soldr-broker.exe").expect("baseline");
        for spelling in [
            r"c:/users/me/soldr-broker.EXE",
            r"\\?\C:\Users\.\Me\soldr-broker.ExE",
            r"C:\\Users\Me\.\soldr-broker.exe",
        ] {
            assert_eq!(
                windows_broker_pipe_from_executable(spelling).expect(spelling),
                expected,
                "{spelling}"
            );
        }
    });

    crate::timed_test!(windows_sanitizer_normalizes_extended_unc, {
        let ordinary = windows_broker_pipe_from_executable(
            r"\\server\profiles\Me\.soldr\broker\soldr-broker.exe",
        )
        .expect("ordinary");
        let extended = windows_broker_pipe_from_executable(
            r"\\?\UNC\SERVER\profiles\me\.soldr\broker\soldr-broker.EXE",
        )
        .expect("extended");
        assert_eq!(ordinary, extended);
        assert!(ordinary
            .logical_socket_path
            .starts_with(r"\\server\profiles"));
    });

    crate::timed_test!(
        windows_sanitizer_encodes_space_percent_and_non_ascii_bytes,
        {
            let endpoint =
                windows_broker_pipe_from_executable("C:\\Users\\Jöhn 100%\\soldr-broker.exe")
                    .expect("endpoint");
            assert!(endpoint.pipe_leaf.contains("%20"));
            assert!(endpoint.pipe_leaf.contains("%25"));
            assert!(endpoint.pipe_leaf.contains("%C3%B6"));
            assert_eq!(
                endpoint.logical_socket_path, "c:\\users\\jöhn 100%\\soldr-broker.sock",
                "non-ASCII case is preserved while ASCII case folds"
            );
        }
    );

    crate::timed_test!(windows_sanitizer_rejects_relative_and_parent_paths, {
        assert!(matches!(
            windows_broker_pipe_from_executable(r"Users\me\soldr-broker.exe"),
            Err(BrokerIdentityError::RelativeWindowsExecutable(_))
        ));
        assert!(matches!(
            windows_broker_pipe_from_executable(r"C:\Users\me\..\other\soldr-broker.exe"),
            Err(BrokerIdentityError::WindowsParentComponent(_))
        ));
    });

    crate::timed_test!(windows_overflow_fallback_is_deterministic_and_diagnostic, {
        let path = format!(r"C:\Users\{}\soldr-broker.exe", "long-profile-".repeat(30));
        let first = windows_broker_pipe_from_executable(&path).expect("first");
        let second = windows_broker_pipe_from_executable(&path).expect("second");
        assert_eq!(first, second);
        assert!(first.overflowed);
        assert!(first.pipe_leaf.starts_with("soldr-broker-ovf-"));
        assert_eq!(first.pipe_leaf.len(), "soldr-broker-ovf-".len() + 16);
        assert!(first.oversized_leaf.as_ref().is_some_and(|leaf| {
            WINDOWS_PIPE_PREFIX.len() + leaf.len() > WINDOWS_PIPE_NAME_LIMIT
        }));
    });

    crate::timed_test!(
        distinct_canonical_windows_paths_have_distinct_regular_leaves,
        {
            let cases = [
                r"C:\Users\a\soldr-broker.exe",
                r"C:\Users\b\soldr-broker.exe",
                r"D:\Users\a\soldr-broker.exe",
                r"\\server\share\a\soldr-broker.exe",
                "C:\\Users\\Ä\\soldr-broker.exe",
                "C:\\Users\\ä\\soldr-broker.exe",
            ];
            let mut logical = std::collections::HashSet::new();
            let mut leaves = std::collections::HashSet::new();
            for case in cases {
                let endpoint = windows_broker_pipe_from_executable(case).expect(case);
                assert!(
                    !endpoint.overflowed,
                    "fixture should exercise injective encoding"
                );
                assert!(logical.insert(endpoint.logical_socket_path));
                assert!(leaves.insert(endpoint.pipe_leaf));
            }
        }
    );

    crate::timed_test!(
        different_profiles_produce_different_endpoints_without_sid_suffixes,
        {
            let a = windows_broker_pipe_from_executable(
                r"C:\Users\alice\.soldr\broker\soldr-broker.exe",
            )
            .unwrap();
            let b =
                windows_broker_pipe_from_executable(r"C:\Users\bob\.soldr\broker\soldr-broker.exe")
                    .unwrap();
            assert_ne!(a.pipe_leaf, b.pipe_leaf);
            assert!(!a.pipe_leaf.contains("sid"));
            assert!(!b.pipe_leaf.contains("sid"));
        }
    );

    crate::timed_test!(unix_fallback_order_is_overflow_then_filesystem, {
        #[cfg(unix)]
        {
            let home = Path::new("/very/long/home/profile");
            let runtime = Path::new("/run/user/123");
            let overflow = resolve_unix_for_home(home, runtime, Some(true), 16).unwrap();
            assert_eq!(
                overflow.fallback,
                Some(BrokerEndpointFallback::UnixSunPathOverflow)
            );
            assert!(overflow
                .bind_endpoint
                .starts_with("/run/user/123/soldr/broker/soldr-broker-"));
            assert!(overflow.bind_endpoint.ends_with(".sock"));

            let network = resolve_unix_for_home(home, runtime, Some(true), 4096).unwrap();
            assert_eq!(
                network.fallback,
                Some(BrokerEndpointFallback::UnixNonBindableFilesystem)
            );
            assert_eq!(overflow.logical_socket_path, network.logical_socket_path);
            assert_eq!(overflow.bind_endpoint, network.bind_endpoint);

            let other = resolve_unix_for_home(
                Path::new("/another/very/long/home/profile"),
                runtime,
                Some(true),
                16,
            )
            .unwrap();
            assert_ne!(overflow.bind_endpoint, other.bind_endpoint);
        }
    });

    crate::timed_test!(endpoint_identity_contains_no_route_or_version_inputs, {
        let first =
            windows_broker_pipe_from_executable(r"C:\Users\same\.soldr\broker\soldr-broker.exe")
                .unwrap();
        let second =
            windows_broker_pipe_from_executable(r"C:\Users\same\.soldr\broker\soldr-broker.exe")
                .unwrap();
        assert_eq!(first, second);
        for forbidden in ["rpb-v2", "soldr-daemon", "0.9", "route", "session-1"] {
            assert!(!first.pipe_leaf.contains(forbidden));
        }
    });
}
