//! zccache integration surface for soldr.
//!
//! soldr owns the build UX and cache policy, while zccache provides the actual
//! compiler-cache engine and daemon.

use crate::core::SoldrPaths;
use serde::Deserialize;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub mod auto_gc;
pub mod build_active;
pub mod cargo_global_cache;
pub mod cargo_lock;
pub mod cook_archive;
pub mod cook_gc;
pub mod cook_index;
pub mod gc;
pub mod path_safety;
pub mod pep517_gc;
pub mod prune_target;
pub mod redb_lock;
pub mod save;
pub mod state_db;
pub mod strip_target;
pub mod target_registry;
pub mod trash_gc;

/// Directory for the auto-GC structured log (`~/.soldr/logs/auto-gc.log`).
pub fn auto_gc_log_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("logs").join("auto-gc.log")
}

/// Marker file used to throttle the auto-GC check so we don't hammer
/// the cargo `.package-cache` lock on every rustc-wrapper invocation.
pub fn auto_gc_throttle_marker_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(".auto_gc_marker")
}

/// Path to the soldr state database (`~/.soldr/state.redb`) used by the
/// target-directory registry.
pub fn data_db_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(target_registry::DATA_DB_FILE)
}

/// Path to the redb-backed soldr state database (`~/.soldr/state.redb`).
pub fn state_db_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(state_db::STATE_DB_FILE)
}

/// Throttle marker for the once-per-day stale-target startup warning.
pub fn gc_warning_marker_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(".gc_warning_marker")
}

/// Directory for root-scoped GC error logs.
pub fn gc_log_dir(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("logs").join("gc")
}

/// Directory that holds soldr-daemon's IPC endpoint, PID file, and logs.
///
/// soldr#2352: for DEV builds this is namespaced by a per-version stamp
/// (`soldr-daemon/dev-<stamp>/`) so two soldr versions sharing `~/.soldr-dev`
/// get independent pid files / lifecycle logs / unix sockets and never displace
/// each other's daemon. The displace-stale check keys on the pid file, so the
/// endpoint alone (the pipe name) is not enough — the whole identity must move.
/// Official builds keep the bare `soldr-daemon/` (single-daemon prod semantics).
/// `dev_daemon_stamp` is a pure hash (no I/O), so this stays a path computation.
pub fn soldr_daemon_dir(paths: &SoldrPaths) -> PathBuf {
    let base = paths.cache.join("soldr-daemon");
    match dev_daemon_stamp(paths) {
        Some(stamp) => base.join(format!("dev-{stamp}")),
        None => base,
    }
}

/// Unix-domain-socket path used by soldr-daemon on Unix. On Windows the
/// daemon listens on a named pipe instead — see `daemon_pipe_name`.
///
/// macOS caps `sockaddr_un.sun_path` at 104 bytes (Linux: 108). When the
/// natural cache-derived path would exceed that, fall back to a short
/// hash-derived path under `$TMPDIR` (default `/tmp`) keyed on the cache
/// root so two `SOLDR_CACHE_DIR`s can't collide. The PID file + log
/// paths stay under the cache root — only the socket needs a length cap.
pub fn daemon_sock_path(paths: &SoldrPaths) -> PathBuf {
    let preferred = soldr_daemon_dir(paths).join("sock");
    if cfg!(unix) && preferred.as_os_str().len() > 100 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        paths.cache.hash(&mut hasher);
        let suffix = format!("{:016x}", hasher.finish());
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        return tmp.join(format!("sd-{}.sock", &suffix[..12]));
    }
    preferred
}

/// PID + active-binary file written by soldr-daemon. Readers verify the
/// PID is still alive AND that the exe stem matches `soldr-daemon` so a
/// recycled PID can't be mistaken for a live daemon.
pub fn daemon_pid_path(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("daemon.pid")
}

/// Append-only JSONL lifecycle log: spawn, died-idle, died-shutdown.
pub fn daemon_lifecycle_log_path(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("lifecycle.jsonl")
}

/// Catch-all stderr log for the detached daemon (tracing + panics).
/// Reserved for Phase 2 when the soldr-daemon bin redirects its stderr
/// here on detached spawn; for now the helper exists so callers can
/// settle on a stable path without changing call sites later.
#[allow(dead_code)]
pub fn daemon_stderr_log_path(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("daemon.log")
}

/// Compose the pipe name from an opaque OS identity and the cache root.
///
/// Platform-neutral and pure so the naming rules are testable everywhere, not
/// only on Windows (soldr#1808 Workstream 2). Both inputs are hashed — a raw
/// SID never reaches a pipe name, a log line, or an error message.
///
/// Output is `soldr-daemon-<identity>-<cache>`: 13 + 12 + 1 + 12 = 38 ASCII
/// characters, well inside the ~256 limit for `\\.\pipe\<name>`.
#[must_use]
pub fn compose_daemon_pipe_name(user_identity: &[u8], cache_root: &Path) -> String {
    use std::hash::{Hash, Hasher};

    fn short_hash(feed: impl FnOnce(&mut std::collections::hash_map::DefaultHasher)) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        feed(&mut hasher);
        format!("{:016x}", hasher.finish())[..12].to_string()
    }

    let identity = short_hash(|h| user_identity.hash(h));
    let cache = short_hash(|h| cache_root.hash(h));
    format!("soldr-daemon-{identity}-{cache}")
}

/// Stable Windows named-pipe name, derived from the process token's user SID
/// and a hash of the cache root.
///
/// soldr#1808: this used `std::env::var("USERNAME")` with a
/// `unwrap_or_else(|_| "soldr")` fallback, and both halves were bugs. The pipe
/// name is the rendezvous point between client and daemon, so deriving it from
/// a *mutable environment variable* meant a scrubbed or differing `USERNAME`
/// put the two on different pipes — and the fallback was a shared literal, so
/// every user on a host with `USERNAME` unset collapsed onto one name for a
/// given cache root.
///
/// The SID comes from the process token, which an environment scrub cannot
/// change. A lookup failure is an error, never a fallback: a collision-prone
/// default is what produced the original incident.
#[cfg(windows)]
pub fn daemon_pipe_name(paths: &SoldrPaths) -> Result<String, String> {
    let identity = windows_user_identity()?;
    let base = compose_daemon_pipe_name(&identity, &paths.cache);
    // soldr#2352 slice 1: version/install-isolate the DEV daemon so two soldr
    // builds sharing `~/.soldr-dev` don't rendezvous on one pipe and displace
    // each other as "stale-version" every call. Official builds keep the bare
    // name (single-daemon prod semantics on `~/.soldr`).
    Ok(match dev_daemon_stamp(paths) {
        Some(stamp) => format!("{base}-{stamp}"),
        None => base,
    })
}

/// A per-version stamp for the DEV daemon identity (soldr#2352), or `None` for
/// official builds. It is a deterministic 16-hex hash of the versioned root
/// (`~/.soldr-dev/v<X.Y.Z>`): distinct soldr versions yield distinct stamps, so
/// their daemons never share a pid file or pipe — while the client and the
/// same-version daemon it spawns derive the identical value with **no shared
/// file and no I/O** (important, because [`soldr_daemon_dir`] is called widely
/// and must stay a pure path computation).
///
/// Official builds return `None`, keeping the bare `soldr-daemon/` dir + pipe so
/// a newer release still displaces the old daemon on upgrade (prod semantics).
///
/// Isolating same-*version* rebuilds (a persisted random per install, read by
/// both sides from `<versioned_root>/daemon-id`) is a documented follow-up in
/// soldr#2352; this deterministic-per-version stamp is the side-effect-free
/// foundation that fixes the reported cross-version thrash.
fn dev_daemon_stamp(paths: &SoldrPaths) -> Option<String> {
    if soldr_core::build_provenance::is_official_build() {
        return None;
    }
    Some(deterministic_dev_stamp(&paths.versioned_root()))
}

fn deterministic_dev_stamp(versioned_root: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    versioned_root.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod dev_daemon_stamp_tests {
    use super::*;

    fn stamp_is_valid(s: &str) -> bool {
        s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }

    crate::timed_test!(dev_stamp_is_deterministic_hex_and_version_scoped, {
        // Same versioned root -> identical stamp (client + same-version daemon
        // agree with no shared file); different versions -> different stamps.
        let a = deterministic_dev_stamp(Path::new("/home/u/.soldr-dev/v1.0.0"));
        let b = deterministic_dev_stamp(Path::new("/home/u/.soldr-dev/v1.0.0"));
        let c = deterministic_dev_stamp(Path::new("/home/u/.soldr-dev/v2.0.0"));
        assert!(stamp_is_valid(&a), "stamp must be 16 hex: {a}");
        assert_eq!(a, b, "same version must agree");
        assert_ne!(a, c, "distinct versions must not collide");
    });

    crate::timed_test!(dev_build_stamps_the_daemon_dir_distinctly_per_version, {
        // The test binary is a dev (non-official) build, so the daemon dir is
        // namespaced by the per-version stamp — the pid file / lifecycle / sock
        // then live under distinct dirs for distinct versions (soldr#2352).
        let p1 = crate::core::SoldrPaths::with_root(Path::new("/x/.soldr-dev").into());
        let dir = soldr_daemon_dir(&p1);
        let s = dir.to_string_lossy().replace('\\', "/");
        assert!(
            s.contains("/soldr-daemon/dev-"),
            "dev daemon dir must carry the stamp segment: {s}"
        );
        // pid + sock inherit the stamped dir automatically.
        assert!(daemon_pid_path(&p1)
            .to_string_lossy()
            .replace('\\', "/")
            .contains("/soldr-daemon/dev-"));
    });

    #[cfg(windows)]
    crate::timed_test!(dev_build_pipe_carries_the_same_stamp_as_the_dir, {
        let paths = crate::core::SoldrPaths::with_root(std::env::temp_dir().join("wg2352"));
        let stamp = dev_daemon_stamp(&paths).expect("dev build must produce a stamp");
        assert!(stamp_is_valid(&stamp));
        let pipe = daemon_pipe_name(&paths).expect("pipe name");
        assert!(
            pipe.ends_with(&format!("-{stamp}")),
            "pipe must carry the same stamp as the dir: {pipe}"
        );
    });
}

/// Opaque, stable per-user identity: the raw bytes of the current process
/// token's user SID.
///
/// Returned as bytes rather than a string because the caller only hashes it —
/// there is no reason to render a SID anywhere it could be logged.
#[cfg(windows)]
fn windows_user_identity() -> Result<Vec<u8>, String> {
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(clippy::upper_case_acronyms)]
    type HANDLE = *mut std::ffi::c_void;

    const TOKEN_QUERY: DWORD = 0x0008;
    /// `TOKEN_INFORMATION_CLASS::TokenUser`
    const TOKEN_USER_CLASS: i32 = 1;

    extern "system" {
        fn GetCurrentProcess() -> HANDLE;
        fn OpenProcessToken(process: HANDLE, desired_access: DWORD, token: *mut HANDLE) -> BOOL;
        fn GetTokenInformation(
            token: HANDLE,
            class: i32,
            info: *mut std::ffi::c_void,
            info_len: DWORD,
            return_len: *mut DWORD,
        ) -> BOOL;
        fn GetLengthSid(sid: *const std::ffi::c_void) -> DWORD;
        fn IsValidSid(sid: *const std::ffi::c_void) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }

    /// Closes the token handle on every exit path, including the early
    /// returns below.
    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a handle `OpenProcessToken` reported success
            // for, and is closed exactly once because this type is not Clone.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that must not be
    // closed. `token` is only read after `OpenProcessToken` reports success.
    let token = unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!(
                "cannot derive the soldr daemon endpoint: OpenProcessToken failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        TokenHandle(token)
    };

    // Two-call pattern: the first call is expected to fail, reporting the
    // buffer size it needs.
    let mut needed: DWORD = 0;
    // SAFETY: a null buffer of length zero is the documented way to ask for
    // the size; the failure is expected and `needed` is the only output read.
    unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(format!(
            "cannot derive the soldr daemon endpoint: GetTokenInformation reported no TokenUser \
             size ({})",
            std::io::Error::last_os_error()
        ));
    }

    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is exactly the `needed` bytes the previous call asked for,
    // and is not read unless this call reports success.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(format!(
            "cannot derive the soldr daemon endpoint: GetTokenInformation(TokenUser) failed ({})",
            std::io::Error::last_os_error()
        ));
    }

    // `TOKEN_USER` begins with `SID_AND_ATTRIBUTES`, whose first field is the
    // `PSID`. Read that pointer rather than assuming the SID sits inline: it
    // is documented to point into this same buffer, but the offset is not
    // guaranteed.
    if buf.len() < std::mem::size_of::<*const std::ffi::c_void>() {
        return Err("cannot derive the soldr daemon endpoint: TokenUser buffer too small".into());
    }
    // SAFETY: the buffer is at least pointer-sized (checked above) and was
    // filled by a successful `GetTokenInformation`, so its leading field is a
    // `PSID` owned by that buffer. Read unaligned because `buf` is a `Vec<u8>`
    // with no pointer-alignment guarantee.
    let sid = unsafe {
        buf.as_ptr()
            .cast::<*const std::ffi::c_void>()
            .read_unaligned()
    };
    if sid.is_null() {
        return Err("cannot derive the soldr daemon endpoint: TokenUser SID is null".into());
    }
    // SAFETY: `sid` came from a successful TokenUser query; `IsValidSid` is
    // the documented way to check it before calling `GetLengthSid`.
    if unsafe { IsValidSid(sid) } == 0 {
        return Err("cannot derive the soldr daemon endpoint: TokenUser SID is invalid".into());
    }
    // SAFETY: validated above, so `GetLengthSid` returns this SID's true size.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    if sid_len == 0 {
        return Err(
            "cannot derive the soldr daemon endpoint: TokenUser SID has zero length".into(),
        );
    }
    // SAFETY: `sid` points at `sid_len` readable bytes inside `buf`, which is
    // still alive here; the slice is copied before `buf` is dropped.
    Ok(unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), sid_len) }.to_vec())
}

/// Environment variable used to carry cache enable/disable state from the
/// front-door cargo command into child processes.
pub const CACHE_ENABLED_ENV_VAR: &str = "SOLDR_CACHE_ENABLED";

/// Encoded environment value for an enabled cache invocation.
pub const CACHE_ENABLED_VALUE: &str = "1";

/// Encoded environment value for a disabled cache invocation.
pub const CACHE_DISABLED_VALUE: &str = "0";

/// Per-build session identifier recognized by zccache.
pub const ZCCACHE_SESSION_ID_ENV_VAR: &str = "ZCCACHE_SESSION_ID";

/// soldr's per-build session id, propagated from the cargo front door
/// into every wrapper invocation so the daemon can correlate per-crate
/// `RecordCompile` events to a single build session.
pub const SOLDR_BUILD_SESSION_ID_ENV_VAR: &str = "SOLDR_BUILD_SESSION_ID";

/// Legacy external-zccache binary variable retained for compatibility tests.
/// The normal cache route uses Soldr IPC and the embedded service.
pub const ZCCACHE_BINARY_ENV_VAR: &str = "SOLDR_ZCCACHE_BIN";

/// Supported zccache cache-root override used for Soldr-owned artifact state.
pub const ZCCACHE_CACHE_DIR_ENV_VAR: &str = "ZCCACHE_CACHE_DIR";

/// Internal marker for `ZCCACHE_CACHE_DIR` values that were injected by soldr.
pub const MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR: &str = "SOLDR_MANAGED_ZCCACHE_CACHE_DIR";

/// zccache path-remap mode. `auto` makes zccache normalize absolute source
/// paths inside compiled artifacts so two git worktrees of the same repo
/// can share cache hits. See issue #352.
pub const ZCCACHE_PATH_REMAP_ENV_VAR: &str = "ZCCACHE_PATH_REMAP";

/// zccache worktree-root override used as the logical root for path-remap
/// normalization. soldr injects this with `ZCCACHE_PATH_REMAP=auto` so the
/// default parent-cache path has a stable normalization anchor.
pub const ZCCACHE_WORKTREE_ROOT_ENV_VAR: &str = "ZCCACHE_WORKTREE_ROOT";

/// Soldr-side escape hatch for the default `ZCCACHE_PATH_REMAP=auto`
/// injection. Accepts `auto` (default, equivalent to unset) and `off`.
/// See issue #352.
pub const SOLDR_PATH_REMAP_ENV_VAR: &str = "SOLDR_PATH_REMAP";

pub fn cache_enabled_env_value(enabled: bool) -> &'static str {
    if enabled {
        CACHE_ENABLED_VALUE
    } else {
        CACHE_DISABLED_VALUE
    }
}

pub fn cache_enabled_from_env_var(value: Option<&OsStr>) -> bool {
    match value.and_then(OsStr::to_str) {
        None => true,
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
    }
}

pub fn cache_enabled_in_current_process() -> bool {
    cache_enabled_from_env_var(std::env::var_os(CACHE_ENABLED_ENV_VAR).as_deref())
}

pub fn zccache_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("zccache")
}

pub fn sccache_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("sccache")
}

pub fn parse_zccache_session_id(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(response) = serde_json::from_str::<SessionStartResponse>(trimmed) {
        if !response.session_id.trim().is_empty() {
            return Some(response.session_id);
        }
    }

    for line in trimmed.lines() {
        let line = line.trim();
        for prefix in [
            "ZCCACHE_SESSION_ID=",
            "export ZCCACHE_SESSION_ID=",
            "$env:ZCCACHE_SESSION_ID=",
        ] {
            if let Some(value) = line.strip_prefix(prefix) {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

pub fn session_journal_path(zccache_dir: &Path) -> PathBuf {
    zccache_dir.join("logs").join("last-session.jsonl")
}

pub fn session_log_path(zccache_dir: &Path) -> PathBuf {
    zccache_dir.join("logs").join("last-session.log")
}

pub fn session_stats_path(zccache_dir: &Path) -> PathBuf {
    zccache_dir.join("logs").join("last-session-stats.json")
}

#[derive(Debug, Deserialize)]
struct SessionStartResponse {
    session_id: String,
}

#[cfg(test)]
mod tests {
    use super::{
        cache_enabled_env_value, cache_enabled_from_env_var, compose_daemon_pipe_name,
        parse_zccache_session_id, sccache_dir, session_journal_path, session_log_path,
        session_stats_path, zccache_dir, CACHE_DISABLED_VALUE, CACHE_ENABLED_VALUE,
    };
    use crate::core::SoldrPaths;
    use std::{ffi::OsStr, path::Path};

    #[test]
    fn cache_defaults_to_enabled_when_env_is_missing() {
        assert!(cache_enabled_from_env_var(None));
    }

    #[test]
    fn cache_control_parses_common_false_values() {
        for value in ["0", "false", "FALSE", "no", "off", ""] {
            assert!(
                !cache_enabled_from_env_var(Some(OsStr::new(value))),
                "expected {value:?} to disable cache"
            );
        }
    }

    #[test]
    fn cache_control_treats_other_values_as_enabled() {
        for value in ["1", "true", "yes", "unexpected"] {
            assert!(
                cache_enabled_from_env_var(Some(OsStr::new(value))),
                "expected {value:?} to enable cache"
            );
        }
    }

    #[test]
    fn cache_control_serializes_boolean_state() {
        assert_eq!(cache_enabled_env_value(true), CACHE_ENABLED_VALUE);
        assert_eq!(cache_enabled_env_value(false), CACHE_DISABLED_VALUE);
    }

    #[test]
    fn zccache_dir_lives_under_soldr_cache_root() {
        let paths = SoldrPaths::with_root(Path::new("C:\\soldr-root").to_path_buf());
        assert_eq!(
            zccache_dir(&paths),
            paths.root.join("cache").join("zccache")
        );
    }

    #[test]
    fn sccache_dir_lives_under_soldr_cache_root() {
        let paths = SoldrPaths::with_root(Path::new("C:\\soldr-root").to_path_buf());
        assert_eq!(
            sccache_dir(&paths),
            paths.root.join("cache").join("sccache")
        );
    }

    #[test]
    fn parses_json_session_start_output() {
        let session_id = parse_zccache_session_id(
            r#"{"session_id":"08f063c0-5f01-4c92-aec1-3f304d9224d0","started_at":1776141813}"#,
        );
        assert_eq!(
            session_id.as_deref(),
            Some("08f063c0-5f01-4c92-aec1-3f304d9224d0")
        );
    }

    #[test]
    fn parses_shell_style_session_start_output() {
        let session_id = parse_zccache_session_id(
            "export ZCCACHE_SESSION_ID=08f063c0-5f01-4c92-aec1-3f304d9224d0",
        );
        assert_eq!(
            session_id.as_deref(),
            Some("08f063c0-5f01-4c92-aec1-3f304d9224d0")
        );
    }

    #[test]
    fn session_journal_path_uses_logs_directory() {
        let path = session_journal_path(Path::new("C:\\soldr-root\\cache\\zccache"));
        assert_eq!(
            path,
            Path::new("C:\\soldr-root\\cache\\zccache")
                .join("logs")
                .join("last-session.jsonl")
        );
    }

    #[test]
    fn session_log_path_uses_logs_directory() {
        let path = session_log_path(Path::new("C:\\soldr-root\\cache\\zccache"));
        assert_eq!(
            path,
            Path::new("C:\\soldr-root\\cache\\zccache")
                .join("logs")
                .join("last-session.log")
        );
    }

    #[test]
    fn session_stats_path_uses_logs_directory() {
        let path = session_stats_path(Path::new("C:\\soldr-root\\cache\\zccache"));
        assert_eq!(
            path,
            Path::new("C:\\soldr-root\\cache\\zccache")
                .join("logs")
                .join("last-session-stats.json")
        );
    }

    // ---------------------------------------------------------------------
    // soldr#1808 Workstream 2 — pipe-name composition
    // ---------------------------------------------------------------------

    const SID_A: &[u8] = &[1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 111];
    const SID_B: &[u8] = &[1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 222];

    #[test]
    fn pipe_name_is_stable_for_the_same_identity_and_root() {
        let root = Path::new("C:/cache/root");
        assert_eq!(
            compose_daemon_pipe_name(SID_A, root),
            compose_daemon_pipe_name(SID_A, root),
            "the name must not vary between two computations in the same run"
        );
    }

    #[test]
    fn pipe_name_separates_users_and_cache_roots() {
        let root_a = Path::new("C:/cache/a");
        let root_b = Path::new("C:/cache/b");

        // Different users on one cache root must not share a pipe. This is the
        // case the old `USERNAME` fallback collapsed: with the variable unset
        // every user became the literal "soldr".
        assert_ne!(
            compose_daemon_pipe_name(SID_A, root_a),
            compose_daemon_pipe_name(SID_B, root_a)
        );
        // One user with two cache roots must not either -- that separation is
        // what the original hash was for and must survive this change.
        assert_ne!(
            compose_daemon_pipe_name(SID_A, root_a),
            compose_daemon_pipe_name(SID_A, root_b)
        );
    }

    #[test]
    fn pipe_name_is_bounded_ascii_with_no_identity_leak() {
        let name = compose_daemon_pipe_name(SID_A, Path::new("C:/cache/root"));

        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "pipe names must stay in [A-Za-z0-9-]: {name}"
        );
        // `\.\pipe\` plus the name must clear the ~256 char limit with room
        // to spare; this is fixed-width by construction.
        assert_eq!(name.len(), 38, "unexpected width: {name}");

        // The raw SID must never be recoverable from the name -- it is hashed
        // precisely so it cannot reach a log line or an error message.
        let sid_digits: String = SID_A.iter().map(|b| b.to_string()).collect();
        assert!(!name.contains(&sid_digits));
    }

    #[test]
    fn pipe_name_ignores_the_username_environment_variable() {
        // The regression this closes: the name used to be derived from
        // `USERNAME`, so scrubbing or changing it moved the endpoint and the
        // client dialed a pipe the daemon was not serving.
        let root = Path::new("C:/cache/root");
        let baseline = compose_daemon_pipe_name(SID_A, root);
        assert_eq!(
            baseline,
            compose_daemon_pipe_name(SID_A, root),
            "composition takes no environment input at all"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_identity_tests {
    use super::{compose_daemon_pipe_name, windows_user_identity};

    /// soldr#1808: the whole point of the change. Proven on a real token,
    /// which is why this test is Windows-gated rather than mocked -- the
    /// mocked half lives in `compose_daemon_pipe_name`'s tests.
    #[test]
    fn identity_is_a_real_sid_and_ignores_username() {
        let identity = windows_user_identity().expect("own process token must be readable");
        // A well-formed SID: revision byte 1, then a sub-authority count.
        assert_eq!(identity[0], 1, "SID revision byte");
        assert!(
            identity.len() >= 8,
            "SID shorter than its header: {identity:?}"
        );

        // The regression: the endpoint used to move when USERNAME did.
        let root = std::path::Path::new("C:/cache/root");
        let baseline = compose_daemon_pipe_name(&identity, root);
        let again = compose_daemon_pipe_name(
            &windows_user_identity().expect("token readable twice"),
            root,
        );
        assert_eq!(
            baseline, again,
            "the same process must derive the same endpoint on every call"
        );
    }
}
