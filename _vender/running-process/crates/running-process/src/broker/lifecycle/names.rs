//! Canonical v1 broker pipe-name derivation.
//!
//! Phase 1 of #228 (issue #230). Every name is derived from the
//! caller's [`user_sid_hash`](super::sid::user_sid_hash) plus a few
//! frozen string templates. The Windows form is a named pipe
//! (`\\.\pipe\...`); the Unix form is a filesystem socket path under
//! the broker shadow directory.
//!
//! The four canonical names exposed here are:
//!
//! | Function                  | Purpose                                                             |
//! |---------------------------|---------------------------------------------------------------------|
//! | [`shared_broker_pipe`]    | Single per-user broker that serves every service together.          |
//! | [`private_broker_pipe`]   | Service-isolated broker (e.g. one zccache instance only).           |
//! | [`explicit_instance_pipe`]| Hand-named broker for tests/dev/multi-instance scenarios.           |
//! | [`backend_pipe`]          | The per-backend handle the broker hands a client after negotiation. |
//!
//! ## Validation
//!
//! Service names must match `[a-z0-9-]{1,64}`. Version strings must
//! match a semver-like `^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$`.
//! Explicit instance names match `[a-z0-9-]{1,64}`. Case-only
//! collisions (`Zccache` vs `zccache`) are rejected with
//! [`PipePathError::InvalidName`] because Windows named pipes are
//! case-insensitive and silently coalescing would let a malicious
//! caller hijack a legitimate broker.
//!
//! ## Length limits
//!
//! - Windows `\\.\pipe\` names without the `\\?\` long-path prefix
//!   are capped by `MAX_PATH = 260` characters.
//! - macOS `sun_path` (the path field of `struct sockaddr_un`) is 104
//!   bytes. The Unix path returned here is validated to stay under
//!   that bound after combining `shadow_dir() + "/broker/" + name +
//!   ".sock"`.

use std::path::PathBuf;

#[cfg(unix)]
use crate::broker::lifecycle::sid::hash_to_16_hex;
use crate::broker::lifecycle::sid::SidError;

/// Errors that prevent computing a valid pipe path.
#[derive(Debug, thiserror::Error)]
pub enum PipePathError {
    /// A name argument failed regex validation.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName {
        /// The offending input.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The derived path exceeds a platform-specific bound.
    #[error("derived path exceeds {limit_label} ({len} > {max})")]
    PathTooLong {
        /// Length we tried to produce.
        len: usize,
        /// Platform-specific cap.
        max: usize,
        /// "Windows MAX_PATH" / "macOS sun_path" / etc.
        limit_label: &'static str,
    },

    /// Failure to compute the per-user SID hash.
    #[error(transparent)]
    Sid(#[from] SidError),
}

/// A pipe address in platform-neutral form.
///
/// Exactly one of [`Self::windows`] or [`Self::unix`] is populated on
/// any given host. The other field is `None`. Callers select the
/// active platform's value via `cfg(windows)` / `cfg(unix)` blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipePath {
    /// Windows named-pipe path (e.g. `\\.\pipe\rpb-v1-abc-shared`).
    pub windows: Option<String>,
    /// Unix domain socket path (e.g.
    /// `/run/user/1000/running-process/broker/rpb-v1-abc-shared.sock`).
    pub unix: Option<PathBuf>,
}

/// Windows MAX_PATH ceiling without the `\\?\` long-path prefix.
pub const WINDOWS_MAX_PATH: usize = 260;

/// macOS `sun_path` field ceiling. POSIX requires at least 92;
/// Darwin's `struct sockaddr_un` actually has 104.
pub const MACOS_SUN_PATH_MAX: usize = 104;

/// Linux `sun_path` field ceiling. glibc defines it as 108.
pub const LINUX_SUN_PATH_MAX: usize = 108;

/// Compile-time prefix every broker pipe shares. Encodes the v1
/// envelope version and the "running-process broker" namespace so
/// pipe names cannot accidentally collide with anything else under
/// `\\.\pipe\` or `shadow_dir()/broker/`.
const PIPE_PREFIX: &str = "rpb-v1";

/// Compute the shared-broker pipe address.
///
/// The shared broker is the default: one instance per user that fans
/// every service request out to the right backend.
pub fn shared_broker_pipe(user_sid_hash: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-shared"))
}

/// Compute the private-broker pipe address for a single service.
///
/// Service names must match `[a-z0-9-]{1,64}`.
pub fn private_broker_pipe(user_sid_hash: &str, service: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    validate_service_name(service)?;
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-svc-{service}"))
}

/// Compute the explicit-instance broker pipe address.
///
/// `name` must match `[a-z0-9-]{1,64}` and is otherwise unrestricted.
/// Used for tests and multi-instance dev setups.
pub fn explicit_instance_pipe(user_sid_hash: &str, name: &str) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    validate_service_name(name)?; // same `[a-z0-9-]{1,64}` rule
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-inst-{name}"))
}

/// Compute the backend pipe address the broker hands a client after
/// Hello negotiation.
///
/// `random128` is a 16-byte (128-bit) random suffix the broker
/// generates per connection. Rendered as lowercase hex to keep the
/// pipe name in the `[a-z0-9-]` charset.
pub fn backend_pipe(user_sid_hash: &str, random128: &[u8; 16]) -> Result<PipePath, PipePathError> {
    validate_sid_hash(user_sid_hash)?;
    let mut suffix = String::with_capacity(32);
    for b in random128 {
        suffix.push(nibble_to_hex(b >> 4));
        suffix.push(nibble_to_hex(b & 0x0F));
    }
    build_pipe_path(&format!("{PIPE_PREFIX}-{user_sid_hash}-be-{suffix}"))
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a service name against `[a-z0-9-]{1,64}`.
///
/// Exposed for callers that want to validate user input before
/// computing the pipe name (so they can surface a friendlier error).
pub fn validate_service_name(name: &str) -> Result<(), PipePathError> {
    if name.is_empty() {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be at least 1 character",
        });
    }
    if name.len() > 64 {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be 64 characters or fewer",
        });
    }
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' | '-' => {}
            'A'..='Z' => {
                // Case-only collision guard — see module docs.
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "uppercase letters are forbidden (case-only \
                             collisions with lowercase names would silently \
                             merge under Windows named-pipe semantics)",
                });
            }
            _ => {
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "only lowercase ASCII letters, digits, and '-' allowed",
                });
            }
        }
    }
    Ok(())
}

/// Validate a semver-like version string against
/// `^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$`.
///
/// Used by callers that want to render `{service}-{version}` into a
/// pipe name themselves (the helpers here keep the name format flat,
/// but the validator is exposed for the broker-side dispatch table).
pub fn validate_version(version: &str) -> Result<(), PipePathError> {
    if version.is_empty() {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version must not be empty",
        });
    }
    // Split off pre-release tail.
    let (core, prerelease) = match version.split_once('-') {
        Some((core, tail)) => (core, Some(tail)),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version core must be MAJOR.MINOR.PATCH",
        });
    }
    for p in &parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "MAJOR/MINOR/PATCH must be non-empty digits",
            });
        }
    }
    if let Some(tail) = prerelease {
        if tail.is_empty() {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "pre-release suffix after '-' must not be empty",
            });
        }
        for c in tail.chars() {
            match c {
                'a'..='z' | '0'..='9' | '.' => {}
                _ => {
                    return Err(PipePathError::InvalidName {
                        name: version.into(),
                        reason: "pre-release tail allows only [a-z0-9.]",
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_sid_hash(s: &str) -> Result<(), PipePathError> {
    if s.len() != 16 {
        return Err(PipePathError::InvalidName {
            name: s.into(),
            reason: "user_sid_hash must be exactly 16 hex characters",
        });
    }
    for c in s.chars() {
        if !(c.is_ascii_digit() || ('a'..='f').contains(&c)) {
            return Err(PipePathError::InvalidName {
                name: s.into(),
                reason: "user_sid_hash must be lowercase hex",
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path assembly
// ---------------------------------------------------------------------------

#[inline]
fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!("nibble out of range"),
    }
}

fn build_pipe_path(name: &str) -> Result<PipePath, PipePathError> {
    #[cfg(windows)]
    {
        let path = format!(r"\\.\pipe\{name}");
        if path.len() > WINDOWS_MAX_PATH {
            return Err(PipePathError::PathTooLong {
                len: path.len(),
                max: WINDOWS_MAX_PATH,
                limit_label: "Windows MAX_PATH",
            });
        }
        Ok(PipePath {
            windows: Some(path),
            unix: None,
        })
    }

    #[cfg(unix)]
    {
        let dir = unix_broker_socket_dir();
        // macOS sun_path is only 104 bytes. Per the #228 spec, every
        // macOS pipe name folds to `{16char-hash}.sock` — there is no
        // budget to embed the full canonical name. Linux gets 108 bytes
        // AND a guaranteed $XDG_RUNTIME_DIR (or short /tmp fallback),
        // so we keep the full name there for debuggability.
        let leaf = if cfg!(target_os = "macos") {
            format!("{}.sock", hash_to_16_hex(name.as_bytes()))
        } else {
            format!("{name}.sock")
        };
        let candidate = dir.join(leaf);
        let candidate_str = candidate.to_string_lossy();
        let limit = if cfg!(target_os = "macos") {
            MACOS_SUN_PATH_MAX
        } else {
            LINUX_SUN_PATH_MAX
        };
        let limit_label = if cfg!(target_os = "macos") {
            "macOS sun_path"
        } else {
            "Linux sun_path"
        };
        // sockaddr_un is NUL-terminated, so the path string itself
        // must be strictly less than the field width.
        if candidate_str.len() >= limit {
            return Err(PipePathError::PathTooLong {
                len: candidate_str.len(),
                max: limit - 1,
                limit_label,
            });
        }
        Ok(PipePath {
            windows: None,
            unix: Some(candidate),
        })
    }
}

#[cfg(unix)]
fn unix_broker_socket_dir() -> PathBuf {
    // We deliberately do NOT call `client::paths::shadow_dir()` here
    // because that function has filesystem side effects
    // (`create_dir_all`). The names module must be pure / no-IO so the
    // hash + length-limit tests stay deterministic. Callers that need
    // to actually bind the socket are expected to create the parent
    // directory themselves.
    #[cfg(target_os = "macos")]
    {
        // Per the #228 spec: macOS has no $XDG_RUNTIME_DIR and a tight
        // 104-byte sun_path. Use `$TMPDIR/.rp-{uid}` so the parent dir
        // stays short enough to leave room for the hashed leaf.
        // `$TMPDIR` on macOS is per-user (e.g. `/var/folders/.../T/`)
        // so the `-{uid}` suffix is technically redundant there, but
        // we keep it so the path is well-formed when `$TMPDIR` is
        // unset (CI containers, restricted launchd contexts) and we
        // fall back to `/tmp`.
        let uid = unsafe { libc::getuid() };
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        tmp.join(format!(".rp-{uid}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(d).join("running-process").join("broker")
        } else {
            // Fallback: /tmp/running-process-{uid}/broker
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/running-process-{uid}/broker"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HASH: &str = "0123456789abcdef";

    #[test]
    fn shared_broker_pipe_builds() {
        let p = shared_broker_pipe(SAMPLE_HASH).expect("shared pipe should build");
        #[cfg(windows)]
        {
            let w = p.windows.expect("windows form populated on Windows");
            assert!(w.starts_with(r"\\.\pipe\rpb-v1-"));
            assert!(w.ends_with("-shared"));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let u = p.unix.expect("unix form populated on Unix");
            let s = u.to_string_lossy();
            assert!(s.contains("rpb-v1-"));
            assert!(s.ends_with("-shared.sock"));
        }
        #[cfg(target_os = "macos")]
        {
            // macOS folds the canonical name into a 16-char hash —
            // the `rpb-v1-...-shared` segment is the *input* to the
            // hash but doesn't survive into the path.
            let u = p.unix.expect("unix form populated on macOS");
            let s = u.to_string_lossy();
            assert!(s.ends_with(".sock"));
        }
    }

    #[test]
    fn private_broker_pipe_rejects_uppercase() {
        let err = private_broker_pipe(SAMPLE_HASH, "Zccache").unwrap_err();
        match err {
            PipePathError::InvalidName { .. } => {}
            _ => panic!("expected InvalidName, got {err:?}"),
        }
    }

    #[test]
    fn validate_version_accepts_semver() {
        validate_version("1.0.0").unwrap();
        validate_version("1.11.20").unwrap();
        validate_version("0.0.1-alpha.1").unwrap();
        validate_version("2.3.4-rc.1.beta").unwrap();
    }

    #[test]
    fn validate_version_rejects_invalid() {
        assert!(validate_version("").is_err());
        assert!(validate_version("1.0").is_err());
        assert!(validate_version("1.0.0.0").is_err());
        assert!(validate_version("1.0.0-").is_err());
        assert!(validate_version("1.0.0-ALPHA").is_err()); // uppercase
        assert!(validate_version("v1.0.0").is_err());
    }

    #[test]
    fn backend_pipe_uses_hex_suffix() {
        let p = backend_pipe(SAMPLE_HASH, &[0xABu8; 16]).expect("backend pipe");
        let s = match (p.windows, p.unix) {
            (Some(w), None) => w,
            (None, Some(u)) => u.to_string_lossy().into_owned(),
            _ => panic!("exactly one form must be populated"),
        };
        // macOS folds the canonical name into a 16-char hash to fit
        // `sun_path` (104 bytes), so the literal `-be-` segment and
        // raw hex suffix don't appear on that platform — we still
        // assert the leaf shape and uniqueness in the integration
        // test `macos_pipe_paths_are_hashed_leaves`.
        #[cfg(not(target_os = "macos"))]
        {
            assert!(s.contains("-be-"));
            assert!(s.contains(&"ab".repeat(16)));
        }
        #[cfg(target_os = "macos")]
        {
            assert!(s.ends_with(".sock"));
        }
    }
}
