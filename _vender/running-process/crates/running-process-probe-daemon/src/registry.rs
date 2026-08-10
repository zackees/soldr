//! The live registration registry.
//!
//! In-memory only. A daemon restart drops every registration and clients
//! re-register — registrations describe *currently live* processes, so
//! persisting them would mean reloading claims that may already be false.
//! (Crash records are durable; that is a later slice.)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use running_process::broker::server::PeerIdentity;

use crate::state::{arm, drop_state, RegState, StateError};

/// Longest accepted executable path.
pub const MAX_EXE_PATH_BYTES: usize = 4096;
/// Longest accepted short string field (app class/name/version, instance).
pub const MAX_SHORT_FIELD_BYTES: usize = 256;
/// Most `supported_ops` entries accepted.
pub const MAX_SUPPORTED_OPS: usize = 64;
/// Most explicit symbol paths accepted at registration.
pub const MAX_SYMBOL_PATHS: usize = 64;
/// Most `env_allowlist` entries accepted.
pub const MAX_ENV_ALLOWLIST: usize = 256;
/// Maximum bytes in one disclosed env key or value.
pub const MAX_ENV_FIELD_BYTES: usize = 4096;
/// Maximum aggregate bytes in explicitly disclosed environment entries.
pub const MAX_DISCLOSED_ENV_BYTES: usize = 64 * 1024;
/// Registration nonces retained for replay detection.
///
/// Bounded on purpose: an unbounded set is a memory-growth lever for anything
/// that can reach the socket. When full, further registrations are refused
/// rather than silently forgetting old nonces, which would reopen the replay
/// window.
pub const MAX_TRACKED_NONCES: usize = 4096;

/// Process identity that survives PID reuse.
///
/// `pid` alone is not an identity — the OS recycles PIDs. `started_at_unix_ms`
/// pins a specific process instance and `boot_id` separates instances across
/// reboots.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessKey {
    /// OS process id.
    pub pid: u32,
    /// Process start time; distinguishes a reused pid from the original.
    pub started_at_unix_ms: u64,
    /// Host boot id; start times restart from the same epoch after a reboot.
    pub boot_id: String,
}

/// Language runtime a registrant declared itself to be.
///
/// This is a *claim*, like `app_name` — the daemon cannot verify it, because a
/// Python process is a native interpreter binary and looks like any other
/// executable from outside. It is recorded so later slices know whether a
/// captured stack needs interpreter frames attached to be readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Runtime {
    /// The registrant declared nothing, or declared a value this daemon does
    /// not know. Treated as "no runtime-specific handling" rather than as an
    /// error — see [`Runtime::from_proto`].
    #[default]
    Unspecified,
    /// Machine frames only.
    Native,
    /// CPython: machine frames with interpreter frames above them.
    Python,
}

impl Runtime {
    /// Map a wire value onto a known runtime.
    ///
    /// An unrecognized value becomes [`Runtime::Unspecified`] rather than
    /// refusing the registration. The proto reserves 3..15 for runtimes that
    /// do not exist yet, so a newer client declaring one of those would
    /// otherwise be unable to register with an older daemon at all — losing
    /// the whole registration over a field that only selects optional
    /// treatment.
    pub fn from_proto(value: i32) -> Self {
        match value {
            1 => Self::Native,
            2 => Self::Python,
            _ => Self::Unspecified,
        }
    }
}

/// What a registrant permits.
#[derive(Clone, Debug, Default)]
pub struct AllowPolicy {
    /// Whether every probe operation is permitted.
    pub allow_all_ops: bool,
    /// Env var names whose *values* may be disclosed. Everything else is
    /// presence-only — process environments routinely carry credentials, so
    /// values are default-deny.
    pub env_allowlist: Vec<String>,
}

/// What a registrant discloses on query surfaces.
#[derive(Clone, Debug, Default)]
pub struct Disclosure {
    /// Whether the executable path may be shown.
    pub expose_exe_path: bool,
    /// Whether the command line may be shown.
    pub expose_cmdline: bool,
    /// Whether env var *names* may be listed.
    pub expose_env_names: bool,
}

/// A validated registration request.
#[derive(Clone, Debug)]
pub struct RegisterRequest {
    /// Claimed process identity.
    pub key: ProcessKey,
    /// Path to the registrant's executable.
    pub exe_path: PathBuf,
    /// Claimed SHA-256 of that executable.
    pub exe_sha256: [u8; 32],
    /// Coarse grouping for cross-instance queries.
    pub app_class: String,
    /// Human-readable application name.
    pub app_name: String,
    /// Application version string.
    pub app_version: String,
    /// Optional instance discriminator.
    pub instance_name: String,
    /// What the registrant permits.
    pub allow_policy: AllowPolicy,
    /// What the registrant discloses.
    pub disclosure: Disclosure,
    /// Working directory copied only after the registrant opted in.
    pub disclosed_cwd: Option<PathBuf>,
    /// Environment values copied by the registrant after explicit opt-in.
    pub disclosed_env: BTreeMap<String, String>,
    /// Single-use registration nonce.
    pub nonce: [u8; 32],
    /// Operations the registrant supports.
    pub supported_ops: Vec<String>,
    /// Language runtime the registrant declared.
    pub runtime: Runtime,
    /// Coarse symbol-source declaration from the wire.
    pub symbol_source: i32,
    /// Optional process-level manifest declaration.
    pub symbol_manifest_path: Option<PathBuf>,
    /// Explicit symbol files or directories declared by the registrant.
    pub symbol_paths: Vec<PathBuf>,
}

/// One registration.
#[derive(Clone, Debug)]
pub struct RegEntry {
    /// Identity this entry describes.
    pub key: ProcessKey,
    /// Current lifecycle state.
    pub state: RegState,
    /// OS-verified peer that opened the connection.
    pub peer: PeerIdentity,
    /// Registrant's executable path.
    pub exe_path: PathBuf,
    /// Coarse grouping for cross-instance queries.
    pub app_class: String,
    /// Human-readable application name.
    pub app_name: String,
    /// Application version.
    pub app_version: String,
    /// Optional instance discriminator.
    pub instance_name: String,
    /// Language runtime the registrant declared.
    pub runtime: Runtime,
    /// Coarse symbol-source declaration from the wire.
    pub symbol_source: i32,
    /// Optional process-level manifest declaration.
    pub symbol_manifest_path: Option<PathBuf>,
    /// Explicit symbol files or directories declared by the registrant.
    pub symbol_paths: Vec<PathBuf>,
    /// What the registrant permits.
    pub allow_policy: AllowPolicy,
    /// Operations the target advertised.
    pub supported_ops: Vec<String>,
    /// What the registrant discloses.
    pub disclosure: Disclosure,
    /// Working directory disclosed at registration, if opted in.
    pub disclosed_cwd: Option<PathBuf>,
    /// Allowlisted environment values disclosed at registration.
    pub disclosed_env: BTreeMap<String, String>,
    /// Wall-clock registration time for query results.
    pub registered_unix_ms: u64,
    /// Connection that owns this registration.
    pub conn_id: u64,
    /// Whether identity verification has succeeded.
    pub identity_verified: bool,
    /// Whether the owning connection is still open.
    pub connection_alive: bool,
    /// When the last heartbeat arrived.
    pub last_heartbeat: Instant,
}

impl RegEntry {
    /// Whether this registrant allows the *value* of `name` to be disclosed.
    ///
    /// Default-deny: absence from the allowlist means no.
    pub fn may_disclose_env(&self, name: &str) -> bool {
        self.allow_policy.env_allowlist.iter().any(|k| k == name)
    }
}

/// Why a registration was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// A field exceeded its cap.
    #[error("field {field} exceeds its limit ({len} > {max})")]
    OversizeField {
        /// Field that was too large.
        field: &'static str,
        /// Observed length.
        len: usize,
        /// Permitted length.
        max: usize,
    },
    /// Symbol-source enum and declarations were inconsistent.
    #[error("invalid symbol source {value}: {reason}")]
    InvalidSymbolSource {
        /// Raw wire enum value.
        value: i32,
        /// Consistency rule that was violated.
        reason: &'static str,
    },
    /// A disclosed value was not explicitly allowlisted by the registrant.
    #[error("environment variable {name} was disclosed without being allowlisted")]
    UndeclaredEnvironment {
        /// Environment variable name.
        name: String,
    },
    /// The nonce was seen before, or the nonce table is full.
    #[error("registration nonce replayed or nonce table full")]
    NonceReplay,
    /// The connecting peer is not the daemon's owner.
    #[error("peer {uid_or_sid} is not the owner of this daemon")]
    PeerRejected {
        /// The rejected peer's uid or SID.
        uid_or_sid: String,
    },
    /// No entry exists for the given key.
    #[error("no registration for this process key")]
    NotRegistered,
    /// A state transition was refused.
    #[error(transparent)]
    State(#[from] StateError),
}

/// Live registrations, keyed by full [`ProcessKey`].
#[derive(Debug)]
pub struct Registry {
    entries: Mutex<HashMap<ProcessKey, RegEntry>>,
    /// Which key currently holds a given pid. Enables O(1) PID-reuse eviction.
    by_pid: Mutex<HashMap<u32, ProcessKey>>,
    seen_nonces: Mutex<HashSet<[u8; 32]>>,
    owner: String,
}

impl Registry {
    /// Create a registry owned by `owner` (a uid or SID).
    pub fn new(owner: String) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            by_pid: Mutex::new(HashMap::new()),
            seen_nonces: Mutex::new(HashSet::new()),
            owner,
        }
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("registry poisoned").len()
    }

    /// Whether the registry holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current state of `key`, if present.
    pub fn state(&self, key: &ProcessKey) -> Option<RegState> {
        self.entries
            .lock()
            .expect("registry poisoned")
            .get(key)
            .map(|e| e.state)
    }

    /// Snapshot of `key`'s entry.
    pub fn get(&self, key: &ProcessKey) -> Option<RegEntry> {
        self.entries
            .lock()
            .expect("registry poisoned")
            .get(key)
            .cloned()
    }

    /// Snapshot all entries without holding the registry lock during a query.
    pub fn snapshot(&self) -> Vec<RegEntry> {
        self.entries
            .lock()
            .expect("registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Which key currently holds `pid`.
    pub fn holder_of_pid(&self, pid: u32) -> Option<ProcessKey> {
        self.by_pid
            .lock()
            .expect("registry poisoned")
            .get(&pid)
            .cloned()
    }

    /// Validate a request and create a `Registering` entry.
    ///
    /// Bounds are checked before anything is stored, and the nonce is consumed
    /// even on a successful path so it can never be reused.
    pub fn begin_register(
        &self,
        req: RegisterRequest,
        peer: PeerIdentity,
        conn_id: u64,
    ) -> Result<ProcessKey, RegisterError> {
        // Defense in depth: the listener ACL already filters by owner, but a
        // future ingress (HTTP) might not, and this is the last gate before an
        // entry exists.
        if peer.uid_or_sid != self.owner {
            return Err(RegisterError::PeerRejected {
                uid_or_sid: peer.uid_or_sid,
            });
        }

        check_len(
            "exe_path",
            req.exe_path.as_os_str().len(),
            MAX_EXE_PATH_BYTES,
        )?;
        check_len("app_class", req.app_class.len(), MAX_SHORT_FIELD_BYTES)?;
        check_len("app_name", req.app_name.len(), MAX_SHORT_FIELD_BYTES)?;
        check_len("app_version", req.app_version.len(), MAX_SHORT_FIELD_BYTES)?;
        check_len(
            "instance_name",
            req.instance_name.len(),
            MAX_SHORT_FIELD_BYTES,
        )?;
        check_len("boot_id", req.key.boot_id.len(), MAX_SHORT_FIELD_BYTES)?;
        check_len("supported_ops", req.supported_ops.len(), MAX_SUPPORTED_OPS)?;
        check_len("symbol_paths", req.symbol_paths.len(), MAX_SYMBOL_PATHS)?;
        if let Some(path) = &req.symbol_manifest_path {
            check_len(
                "symbol_manifest_path",
                path.as_os_str().len(),
                MAX_EXE_PATH_BYTES,
            )?;
        }
        for path in &req.symbol_paths {
            check_len("symbol_path", path.as_os_str().len(), MAX_EXE_PATH_BYTES)?;
        }
        match req.symbol_source {
            // Backward compatibility: registrations from before #638 carry
            // the proto default and no declarations. They still get the
            // ordinary adjacent/cache lookup.
            0 if req.symbol_manifest_path.is_none() && req.symbol_paths.is_empty() => {}
            1 if req.symbol_manifest_path.is_none() && req.symbol_paths.is_empty() => {}
            2 if req.symbol_manifest_path.is_none() => {}
            3 if req.symbol_manifest_path.is_some() => {}
            0 => {
                return Err(RegisterError::InvalidSymbolSource {
                    value: 0,
                    reason: "UNSPECIFIED cannot carry symbol declarations",
                });
            }
            1 => {
                return Err(RegisterError::InvalidSymbolSource {
                    value: 1,
                    reason: "NONE cannot carry symbol paths or a manifest",
                });
            }
            2 => {
                return Err(RegisterError::InvalidSymbolSource {
                    value: 2,
                    reason: "LOCAL cannot carry a manifest",
                });
            }
            3 => {
                return Err(RegisterError::InvalidSymbolSource {
                    value: 3,
                    reason: "MANIFEST requires a manifest path",
                });
            }
            source => {
                return Err(RegisterError::InvalidSymbolSource {
                    value: source,
                    reason: "unknown enum value",
                });
            }
        }
        check_len(
            "env_allowlist",
            req.allow_policy.env_allowlist.len(),
            MAX_ENV_ALLOWLIST,
        )?;
        if let Some(cwd) = &req.disclosed_cwd {
            check_len("disclosed_cwd", cwd.as_os_str().len(), MAX_EXE_PATH_BYTES)?;
        }
        let mut disclosed_env_bytes = 0usize;
        for (name, value) in &req.disclosed_env {
            check_len("disclosed_env_key", name.len(), MAX_ENV_FIELD_BYTES)?;
            check_len("disclosed_env_value", value.len(), MAX_ENV_FIELD_BYTES)?;
            if !req
                .allow_policy
                .env_allowlist
                .iter()
                .any(|allowed| allowed == name)
            {
                return Err(RegisterError::UndeclaredEnvironment { name: name.clone() });
            }
            disclosed_env_bytes = disclosed_env_bytes.saturating_add(name.len() + value.len());
        }
        check_len(
            "disclosed_env_bytes",
            disclosed_env_bytes,
            MAX_DISCLOSED_ENV_BYTES,
        )?;

        {
            let mut nonces = self.seen_nonces.lock().expect("registry poisoned");
            // Refuse when full rather than evicting: forgetting a nonce would
            // reopen the replay window it exists to close.
            if nonces.len() >= MAX_TRACKED_NONCES || !nonces.insert(req.nonce) {
                return Err(RegisterError::NonceReplay);
            }
        }

        let mut entries = self.entries.lock().expect("registry poisoned");
        let mut by_pid = self.by_pid.lock().expect("registry poisoned");

        // PID reuse: a different instance now holds this pid, so the previous
        // holder is gone. Evict it outright — leaving it would let queries and
        // operations aimed at the new process hit the old entry's state.
        if let Some(prev) = by_pid.get(&req.key.pid) {
            if prev != &req.key {
                entries.remove(prev);
            }
        }

        let entry = RegEntry {
            key: req.key.clone(),
            state: RegState::Registering,
            peer,
            exe_path: req.exe_path,
            app_class: req.app_class,
            app_name: req.app_name,
            app_version: req.app_version,
            instance_name: req.instance_name,
            runtime: req.runtime,
            symbol_source: req.symbol_source,
            allow_policy: req.allow_policy,
            supported_ops: req.supported_ops,
            disclosure: req.disclosure,
            disclosed_cwd: req.disclosed_cwd,
            disclosed_env: req.disclosed_env,
            registered_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
            symbol_manifest_path: req.symbol_manifest_path,
            symbol_paths: req.symbol_paths,
            conn_id,
            identity_verified: false,
            connection_alive: true,
            last_heartbeat: Instant::now(),
        };

        by_pid.insert(req.key.pid, req.key.clone());
        entries.insert(req.key.clone(), entry);
        Ok(req.key)
    }

    /// Record the outcome of identity verification and attempt to arm.
    ///
    /// Verification itself is performed by the caller (it touches the
    /// filesystem and the process table); this applies the result under the
    /// registry lock so the guard and the state change cannot interleave.
    pub fn verify_and_arm(
        &self,
        key: &ProcessKey,
        identity_verified: bool,
        connection_alive: bool,
    ) -> Result<(), RegisterError> {
        let mut entries = self.entries.lock().expect("registry poisoned");
        let entry = entries.get_mut(key).ok_or(RegisterError::NotRegistered)?;

        entry.identity_verified = identity_verified;
        entry.connection_alive = connection_alive;

        let next = arm(entry.state, entry.identity_verified, entry.connection_alive)?;
        entry.state = next;
        Ok(())
    }

    /// Refresh liveness for `key` on the connection that registered it.
    pub fn heartbeat(&self, key: &ProcessKey, conn_id: u64) -> Result<(), RegisterError> {
        let mut entries = self.entries.lock().expect("registry poisoned");
        let entry = entries.get_mut(key).ok_or(RegisterError::NotRegistered)?;
        if entry.state == RegState::Dropped || entry.conn_id != conn_id {
            return Err(RegisterError::NotRegistered);
        }
        entry.last_heartbeat = Instant::now();
        Ok(())
    }

    /// Drop every registration owned by `conn_id`.
    ///
    /// The primary liveness mechanism: a closed connection means the
    /// registrant is gone *now*, with no grace period.
    pub fn drop_by_conn(&self, conn_id: u64) -> usize {
        let mut entries = self.entries.lock().expect("registry poisoned");
        let mut by_pid = self.by_pid.lock().expect("registry poisoned");

        let doomed: Vec<ProcessKey> = entries
            .values()
            .filter(|e| e.conn_id == conn_id)
            .map(|e| e.key.clone())
            .collect();

        for key in &doomed {
            if let Some(e) = entries.get_mut(key) {
                e.state = drop_state(e.state);
                e.connection_alive = false;
            }
            entries.remove(key);
            if by_pid.get(&key.pid) == Some(key) {
                by_pid.remove(&key.pid);
            }
        }
        doomed.len()
    }

    /// Drop entries whose last heartbeat is older than `grace`.
    ///
    /// Backstop only. It exists for SIGKILL and other exits where no close
    /// ever arrives; a clean disconnect is handled by [`Self::drop_by_conn`]
    /// without waiting.
    pub fn reap_expired(&self, now: Instant, grace: Duration) -> usize {
        let mut entries = self.entries.lock().expect("registry poisoned");
        let mut by_pid = self.by_pid.lock().expect("registry poisoned");

        let doomed: Vec<ProcessKey> = entries
            .values()
            .filter(|e| now.saturating_duration_since(e.last_heartbeat) > grace)
            .map(|e| e.key.clone())
            .collect();

        for key in &doomed {
            entries.remove(key);
            if by_pid.get(&key.pid) == Some(key) {
                by_pid.remove(&key.pid);
            }
        }
        doomed.len()
    }
}

fn check_len(field: &'static str, len: usize, max: usize) -> Result<(), RegisterError> {
    if len > max {
        return Err(RegisterError::OversizeField { field, len, max });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> String {
        "owner-uid".to_string()
    }

    fn peer() -> PeerIdentity {
        PeerIdentity {
            pid: 1234,
            uid_or_sid: owner(),
        }
    }

    fn key(pid: u32, started: u64) -> ProcessKey {
        ProcessKey {
            pid,
            started_at_unix_ms: started,
            boot_id: "boot-1".into(),
        }
    }

    fn req(k: ProcessKey, nonce: u8) -> RegisterRequest {
        RegisterRequest {
            key: k,
            exe_path: PathBuf::from("/usr/bin/app"),
            exe_sha256: [0u8; 32],
            app_class: "clud".into(),
            app_name: "clud".into(),
            app_version: "1.0".into(),
            instance_name: String::new(),
            allow_policy: AllowPolicy::default(),
            disclosure: Disclosure::default(),
            disclosed_cwd: None,
            disclosed_env: Default::default(),
            nonce: [nonce; 32],
            supported_ops: vec![],
            runtime: Runtime::Native,
            symbol_source: 2,
            symbol_manifest_path: None,
            symbol_paths: Vec::new(),
        }
    }

    #[test]
    fn disclosing_an_env_value_outside_the_allowlist_is_refused() {
        // The allowlist is the consent record, and `disclosed_env` is the
        // payload it authorizes. A registrant that ships a value it never
        // allowlisted is refused outright rather than having the extra key
        // quietly dropped: the query surface reads straight out of
        // `disclosed_env`, so anything stored there is disclosable, and the
        // only safe place to stop an unauthorized value is before it lands.
        let reg = Registry::new(owner());
        let mut request = req(key(20, 100), 20);
        request.allow_policy.env_allowlist = vec!["FOO".into()];
        request.disclosed_env.insert("SECRET".into(), "xyz".into());

        let err = reg.begin_register(request, peer(), 1).unwrap_err();
        assert_eq!(
            err,
            RegisterError::UndeclaredEnvironment {
                name: "SECRET".into()
            }
        );
        // And nothing was stored, so no later query can reach it.
        assert!(reg.is_empty());
    }

    #[test]
    fn disclosing_an_allowlisted_env_value_is_accepted() {
        // Control for the test above: same shape, one allowlisted key. If this
        // failed too, the refusal above would prove nothing about the gate.
        let reg = Registry::new(owner());
        let mut request = req(key(21, 100), 21);
        request.allow_policy.env_allowlist = vec!["FOO".into()];
        request.disclosed_env.insert("FOO".into(), "bar".into());

        let k = reg.begin_register(request, peer(), 1).unwrap();
        let entry = reg.get(&k).expect("registration should exist");
        assert_eq!(
            entry.disclosed_env.get("FOO").map(String::as_str),
            Some("bar")
        );
    }

    #[test]
    fn happy_path_registers_then_arms() {
        let reg = Registry::new(owner());
        let k = reg.begin_register(req(key(10, 100), 1), peer(), 1).unwrap();
        assert_eq!(reg.state(&k), Some(RegState::Registering));
        reg.verify_and_arm(&k, true, true).unwrap();
        assert_eq!(reg.state(&k), Some(RegState::Armed));
    }

    #[test]
    fn arming_without_a_live_connection_is_refused() {
        let reg = Registry::new(owner());
        let k = reg.begin_register(req(key(11, 100), 2), peer(), 1).unwrap();
        assert!(matches!(
            reg.verify_and_arm(&k, true, false),
            Err(RegisterError::State(StateError::NoLiveProbe))
        ));
        assert_eq!(reg.state(&k), Some(RegState::Registering));
    }

    #[test]
    fn arming_without_verified_identity_is_refused() {
        let reg = Registry::new(owner());
        let k = reg.begin_register(req(key(12, 100), 3), peer(), 1).unwrap();
        assert!(matches!(
            reg.verify_and_arm(&k, false, true),
            Err(RegisterError::State(StateError::IdentityUnverified))
        ));
    }

    #[test]
    fn foreign_peer_is_refused_and_creates_no_entry() {
        let reg = Registry::new(owner());
        let stranger = PeerIdentity {
            pid: 9,
            uid_or_sid: "someone-else".into(),
        };
        assert!(matches!(
            reg.begin_register(req(key(13, 100), 4), stranger, 1),
            Err(RegisterError::PeerRejected { .. })
        ));
        assert!(reg.is_empty(), "a refused peer must leave no entry behind");
    }

    #[test]
    fn replayed_nonce_is_refused() {
        let reg = Registry::new(owner());
        reg.begin_register(req(key(14, 100), 7), peer(), 1).unwrap();
        assert!(matches!(
            reg.begin_register(req(key(15, 100), 7), peer(), 2),
            Err(RegisterError::NonceReplay)
        ));
    }

    #[test]
    fn oversize_field_is_refused_with_the_offending_name() {
        let reg = Registry::new(owner());
        let mut r = req(key(16, 100), 8);
        r.app_class = "x".repeat(MAX_SHORT_FIELD_BYTES + 1);
        match reg.begin_register(r, peer(), 1) {
            Err(RegisterError::OversizeField { field, .. }) => assert_eq!(field, "app_class"),
            other => panic!("expected OversizeField, got {other:?}"),
        }
        assert!(reg.is_empty());
    }

    #[test]
    fn symbol_source_and_declarations_must_be_consistent() {
        let reg = Registry::new(owner());
        let mut r = req(key(17, 100), 9);
        r.symbol_source = 1;
        r.symbol_paths.push(PathBuf::from("/symbols"));
        assert!(matches!(
            reg.begin_register(r, peer(), 1),
            Err(RegisterError::InvalidSymbolSource { value: 1, .. })
        ));
        assert!(reg.is_empty());
    }

    /// The core PID-reuse property: a recycled pid is a different instance and
    /// must not inherit the previous entry.
    #[test]
    fn reused_pid_evicts_the_previous_instance() {
        let reg = Registry::new(owner());
        let old = reg.begin_register(req(key(20, 100), 9), peer(), 1).unwrap();
        reg.verify_and_arm(&old, true, true).unwrap();

        let new = reg
            .begin_register(req(key(20, 999), 10), peer(), 2)
            .unwrap();

        assert_ne!(old, new, "different start times are different identities");
        assert!(
            reg.get(&old).is_none(),
            "the superseded instance must be evicted, not left behind"
        );
        assert_eq!(reg.holder_of_pid(20), Some(new.clone()));
        assert_eq!(
            reg.state(&new),
            Some(RegState::Registering),
            "the new instance starts cold — it must not inherit Armed"
        );
    }

    #[test]
    fn connection_close_drops_immediately() {
        let reg = Registry::new(owner());
        let k = reg
            .begin_register(req(key(30, 100), 11), peer(), 42)
            .unwrap();
        reg.verify_and_arm(&k, true, true).unwrap();

        assert_eq!(reg.drop_by_conn(42), 1);
        assert_eq!(reg.state(&k), None, "dropped entries are removed at once");
        assert_eq!(reg.holder_of_pid(30), None);
    }

    #[test]
    fn drop_by_conn_only_touches_that_connection() {
        let reg = Registry::new(owner());
        let a = reg
            .begin_register(req(key(31, 100), 12), peer(), 1)
            .unwrap();
        let b = reg
            .begin_register(req(key(32, 100), 13), peer(), 2)
            .unwrap();
        reg.drop_by_conn(1);
        assert!(reg.get(&a).is_none());
        assert!(reg.get(&b).is_some(), "unrelated connection must survive");
    }

    #[test]
    fn heartbeat_lapse_reaps_the_entry() {
        let reg = Registry::new(owner());
        let k = reg
            .begin_register(req(key(40, 100), 14), peer(), 1)
            .unwrap();
        reg.verify_and_arm(&k, true, true).unwrap();

        let grace = Duration::from_secs(15);
        assert_eq!(reg.reap_expired(Instant::now(), grace), 0, "still fresh");

        let later = Instant::now() + Duration::from_secs(16);
        assert_eq!(reg.reap_expired(later, grace), 1);
        assert_eq!(reg.state(&k), None);
    }

    #[test]
    fn heartbeat_refreshes_liveness() {
        let reg = Registry::new(owner());
        let k = reg
            .begin_register(req(key(41, 100), 15), peer(), 1)
            .unwrap();
        reg.heartbeat(&k, 1).unwrap();
        assert_eq!(reg.reap_expired(Instant::now(), Duration::from_secs(15)), 0);
    }

    #[test]
    fn heartbeat_from_another_connection_is_rejected() {
        let reg = Registry::new(owner());
        let k = reg
            .begin_register(req(key(42, 100), 17), peer(), 1)
            .unwrap();
        assert!(matches!(
            reg.heartbeat(&k, 2),
            Err(RegisterError::NotRegistered)
        ));
    }

    #[test]
    fn heartbeat_for_unknown_key_is_an_error() {
        let reg = Registry::new(owner());
        assert!(matches!(
            reg.heartbeat(&key(99, 1), 7),
            Err(RegisterError::NotRegistered)
        ));
    }

    #[test]
    fn env_values_are_default_deny() {
        let reg = Registry::new(owner());
        let mut r = req(key(50, 100), 16);
        r.allow_policy.env_allowlist = vec!["PATH".into()];
        let k = reg.begin_register(r, peer(), 1).unwrap();
        let entry = reg.get(&k).unwrap();

        assert!(entry.may_disclose_env("PATH"));
        assert!(
            !entry.may_disclose_env("AWS_SECRET_ACCESS_KEY"),
            "anything not allowlisted must be denied"
        );
    }

    #[test]
    fn nonce_table_refuses_rather_than_forgetting() {
        let reg = Registry::new(owner());
        for i in 0..MAX_TRACKED_NONCES {
            let mut nonce = [0u8; 32];
            nonce[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let mut r = req(key(60 + i as u32, 100), 0);
            r.nonce = nonce;
            reg.begin_register(r, peer(), 1).unwrap();
        }
        let mut r = req(key(999_999, 100), 0);
        r.nonce = [0xAB; 32];
        assert!(
            matches!(
                reg.begin_register(r, peer(), 1),
                Err(RegisterError::NonceReplay)
            ),
            "a full nonce table must refuse, never evict"
        );
    }
}
