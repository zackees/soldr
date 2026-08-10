//! Bounded, privacy-preserving queries over live probe registrations.
//!
//! Registered processes disclose cwd and environment values at registration,
//! after explicit application opt-in. The daemon never scrapes their full
//! environment. Forced OS discovery deliberately does not request environment
//! data at all, so an unregistered process cannot leak even variable names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use running_process_probe::probe_diag::v1 as wire;
use sysinfo::{ProcessRefreshKind, System, UpdateKind};

use crate::registry::{ProcessKey, RegEntry, Registry, Runtime};
use crate::state::RegState;

/// OS snapshots are reused briefly to keep broad `--force` queries cheap.
pub const OS_TABLE_TTL: Duration = Duration::from_secs(2);
/// Largest accepted result set.
pub const MAX_QUERY_LIMIT: u32 = 1024;
/// Largest glob or regex accepted from one request.
pub const MAX_SELECTOR_BYTES: usize = 1024;
/// Most environment predicates in one request.
pub const MAX_ENV_MATCHES: usize = 32;
/// Largest environment key in a predicate.
pub const MAX_ENV_KEY_BYTES: usize = 256;

/// A query validation failure.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueryError {
    /// Every query must carry an explicit nonzero limit.
    #[error("query limit is required")]
    MissingLimit,
    /// The requested result bound is too large.
    #[error("query limit {actual} exceeds maximum {max}")]
    LimitTooLarge {
        /// Requested limit.
        actual: u32,
        /// Maximum limit.
        max: u32,
    },
    /// A selector exceeded its byte cap.
    #[error("selector {field} exceeds {max} bytes")]
    SelectorTooLong {
        /// Selector field.
        field: &'static str,
        /// Maximum bytes.
        max: usize,
    },
    /// Two mutually exclusive selector forms were supplied.
    #[error("selector {field} may use only one match kind")]
    ConflictingSelector {
        /// Selector field.
        field: &'static str,
    },
    /// A glob or regex failed to compile.
    #[error("invalid {field} selector: {detail}")]
    InvalidSelector {
        /// Selector field.
        field: &'static str,
        /// Parser detail.
        detail: String,
    },
    /// A numeric range was inverted or out of range.
    #[error("invalid {field} range")]
    InvalidRange {
        /// Range field.
        field: &'static str,
    },
    /// Too many environment predicates were supplied.
    #[error("environment selector count {actual} exceeds maximum {max}")]
    TooManyEnvMatches {
        /// Requested count.
        actual: usize,
        /// Maximum count.
        max: usize,
    },
    /// An environment predicate omitted or oversized its key.
    #[error("invalid environment key")]
    InvalidEnvKey,
}

#[derive(Clone, Debug)]
enum TextMatcher {
    Exact(String),
    Glob(GlobMatcher),
    Regex(Regex),
}

impl TextMatcher {
    fn exact(value: &str, field: &'static str) -> Result<Self, QueryError> {
        check_selector_len(value, field)?;
        Ok(Self::Exact(value.to_owned()))
    }

    fn glob(value: &str, field: &'static str) -> Result<Self, QueryError> {
        check_selector_len(value, field)?;
        GlobBuilder::new(value)
            .literal_separator(false)
            .build()
            .map(|glob| Self::Glob(glob.compile_matcher()))
            .map_err(|error| QueryError::InvalidSelector {
                field,
                detail: error.to_string(),
            })
    }

    fn regex(value: &str, field: &'static str) -> Result<Self, QueryError> {
        check_selector_len(value, field)?;
        Regex::new(value)
            .map(Self::Regex)
            .map_err(|error| QueryError::InvalidSelector {
                field,
                detail: error.to_string(),
            })
    }

    fn is_match(&self, value: &str) -> bool {
        match self {
            Self::Exact(expected) => expected == value,
            Self::Glob(glob) => glob.is_match(value),
            Self::Regex(regex) => regex.is_match(value),
        }
    }
}

fn check_selector_len(value: &str, field: &'static str) -> Result<(), QueryError> {
    if value.len() > MAX_SELECTOR_BYTES {
        Err(QueryError::SelectorTooLong {
            field,
            max: MAX_SELECTOR_BYTES,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct EnvPredicate {
    key: String,
    value: Option<TextMatcher>,
}

/// A validated, compiled process query.
#[derive(Clone, Debug)]
pub struct ProcessQuery {
    name: Option<TextMatcher>,
    exe: Option<TextMatcher>,
    cwd: Option<TextMatcher>,
    app_class: Option<TextMatcher>,
    pid: Option<u32>,
    pid_range: Option<(u32, u32)>,
    start_time_range: Option<(u64, u64)>,
    env: Vec<EnvPredicate>,
    include_env: bool,
    include_unregistered: bool,
    limit: usize,
}

impl ProcessQuery {
    /// Validate and compile a wire query.
    pub fn from_proto(query: wire::ProcessQuery) -> Result<Self, QueryError> {
        if query.limit == 0 {
            return Err(QueryError::MissingLimit);
        }
        if query.limit > MAX_QUERY_LIMIT {
            return Err(QueryError::LimitTooLarge {
                actual: query.limit,
                max: MAX_QUERY_LIMIT,
            });
        }

        let name = compile_pair(
            &query.name_glob,
            &query.name_regex,
            "name",
            TextMatcher::glob,
            TextMatcher::regex,
        )?;
        let exe = compile_pair(
            &query.exe_glob,
            &query.exe_regex,
            "exe",
            TextMatcher::glob,
            TextMatcher::regex,
        )?;
        let cwd = compile_pair(
            &query.cwd_glob,
            &query.cwd_regex,
            "cwd",
            TextMatcher::glob,
            TextMatcher::regex,
        )?;
        let app_class = (!query.app_class.is_empty())
            .then(|| TextMatcher::exact(&query.app_class, "app_class"))
            .transpose()?;

        let pid = query
            .pid
            .map(|value| {
                u32::try_from(value).map_err(|_| QueryError::InvalidRange { field: "pid" })
            })
            .transpose()?;
        let pid_range = numeric_range_u32(query.pid_min, query.pid_max, "pid")?;
        let start_time_range = numeric_range_u64(
            query.start_time_min_unix_ms,
            query.start_time_max_unix_ms,
            "start_time",
        )?;

        if query.env.len() > MAX_ENV_MATCHES {
            return Err(QueryError::TooManyEnvMatches {
                actual: query.env.len(),
                max: MAX_ENV_MATCHES,
            });
        }
        let env = query
            .env
            .into_iter()
            .map(compile_env)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            name,
            exe,
            cwd,
            app_class,
            pid,
            pid_range,
            start_time_range,
            env,
            include_env: query.include_env,
            include_unregistered: query.include_unregistered,
            limit: query.limit as usize,
        })
    }
}

fn compile_pair(
    glob: &str,
    regex: &str,
    field: &'static str,
    compile_glob: fn(&str, &'static str) -> Result<TextMatcher, QueryError>,
    compile_regex: fn(&str, &'static str) -> Result<TextMatcher, QueryError>,
) -> Result<Option<TextMatcher>, QueryError> {
    match (glob.is_empty(), regex.is_empty()) {
        (false, false) => Err(QueryError::ConflictingSelector { field }),
        (false, true) => compile_glob(glob, field).map(Some),
        (true, false) => compile_regex(regex, field).map(Some),
        (true, true) => Ok(None),
    }
}

fn compile_env(env: wire::EnvMatch) -> Result<EnvPredicate, QueryError> {
    if env.key.is_empty() || env.key.len() > MAX_ENV_KEY_BYTES {
        return Err(QueryError::InvalidEnvKey);
    }
    let kinds = usize::from(env.value_exact.is_some())
        + usize::from(!env.value_glob.is_empty())
        + usize::from(!env.value_regex.is_empty());
    if kinds > 1 {
        return Err(QueryError::ConflictingSelector { field: "env" });
    }
    let value = if let Some(exact) = env.value_exact {
        Some(TextMatcher::exact(&exact, "env")?)
    } else if !env.value_glob.is_empty() {
        Some(TextMatcher::glob(&env.value_glob, "env")?)
    } else if !env.value_regex.is_empty() {
        Some(TextMatcher::regex(&env.value_regex, "env")?)
    } else {
        None
    };
    Ok(EnvPredicate {
        key: env.key,
        value,
    })
}

fn numeric_range_u32(
    min: Option<u64>,
    max: Option<u64>,
    field: &'static str,
) -> Result<Option<(u32, u32)>, QueryError> {
    if min.is_none() && max.is_none() {
        return Ok(None);
    }
    let min = u32::try_from(min.unwrap_or(0)).map_err(|_| QueryError::InvalidRange { field })?;
    let max = u32::try_from(max.unwrap_or(u64::from(u32::MAX)))
        .map_err(|_| QueryError::InvalidRange { field })?;
    (min <= max)
        .then_some((min, max))
        .map(Some)
        .ok_or(QueryError::InvalidRange { field })
}

fn numeric_range_u64(
    min: Option<u64>,
    max: Option<u64>,
    field: &'static str,
) -> Result<Option<(u64, u64)>, QueryError> {
    if min.is_none() && max.is_none() {
        return Ok(None);
    }
    let range = (min.unwrap_or(0), max.unwrap_or(u64::MAX));
    (range.0 <= range.1)
        .then_some(range)
        .map(Some)
        .ok_or(QueryError::InvalidRange { field })
}

#[derive(Clone, Debug)]
struct Candidate {
    key: ProcessKey,
    name: String,
    exe: Option<PathBuf>,
    cwd: Option<PathBuf>,
    registered: bool,
    app_class: String,
    app_name: String,
    app_version: String,
    instance_name: String,
    runtime: i32,
    supported_ops: Vec<i32>,
    registered_unix_ms: u64,
    disclosed_env: BTreeMap<String, String>,
    disclose_env_names: bool,
}

impl Candidate {
    fn from_registration(entry: RegEntry) -> Option<Self> {
        if entry.state != RegState::Armed {
            return None;
        }
        let name = entry
            .exe_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| entry.app_name.clone());
        let exe = entry.disclosure.expose_exe_path.then_some(entry.exe_path);
        Some(Self {
            key: entry.key,
            name,
            exe,
            cwd: entry.disclosed_cwd,
            registered: true,
            app_class: entry.app_class,
            app_name: entry.app_name,
            app_version: entry.app_version,
            instance_name: entry.instance_name,
            runtime: match entry.runtime {
                Runtime::Unspecified => wire::Runtime::Unspecified as i32,
                Runtime::Native => wire::Runtime::Native as i32,
                Runtime::Python => wire::Runtime::Python as i32,
            },
            supported_ops: entry
                .supported_ops
                .iter()
                .filter_map(|op| match op.as_str() {
                    "stack_capture" => Some(wire::SupportedOp::StackCapture as i32),
                    "cpu_profile" => Some(wire::SupportedOp::CpuProfile as i32),
                    "heap_profile" => Some(wire::SupportedOp::HeapProfile as i32),
                    "off_cpu_profile" => Some(wire::SupportedOp::OffCpuProfile as i32),
                    _ => None,
                })
                .collect(),
            registered_unix_ms: entry.registered_unix_ms,
            disclosed_env: entry.disclosed_env,
            disclose_env_names: entry.disclosure.expose_env_names,
        })
    }

    fn matches(&self, query: &ProcessQuery) -> bool {
        matches_text(query.name.as_ref(), Some(&self.name))
            && matches_text(
                query.exe.as_ref(),
                self.exe
                    .as_ref()
                    .map(|path| path.to_string_lossy())
                    .as_deref(),
            )
            && matches_text(
                query.cwd.as_ref(),
                self.cwd
                    .as_ref()
                    .map(|path| path.to_string_lossy())
                    .as_deref(),
            )
            && matches_text(
                query.app_class.as_ref(),
                self.registered.then_some(self.app_class.as_str()),
            )
            && query.pid.is_none_or(|pid| self.key.pid == pid)
            && query
                .pid_range
                .is_none_or(|(min, max)| (min..=max).contains(&self.key.pid))
            && query
                .start_time_range
                .is_none_or(|(min, max)| (min..=max).contains(&self.key.started_at_unix_ms))
            && query.env.iter().all(|predicate| {
                if !self.registered {
                    return false;
                }
                let Some(value) = self.disclosed_env.get(&predicate.key) else {
                    return false;
                };
                predicate
                    .value
                    .as_ref()
                    .is_none_or(|matcher| matcher.is_match(value))
            })
    }

    fn into_proto(self, include_env: bool) -> wire::ProcessInfo {
        let env: std::collections::HashMap<String, String> = if include_env {
            self.disclosed_env.into_iter().collect()
        } else {
            Default::default()
        };
        let env_names = if include_env && self.disclose_env_names {
            env.keys().cloned().collect()
        } else {
            Vec::new()
        };
        wire::ProcessInfo {
            key: Some(wire::ProcessKey {
                pid: u64::from(self.key.pid),
                start_time: Some(self.key.started_at_unix_ms),
                boot_id: Some(self.key.boot_id),
            }),
            exe_path: self
                .exe
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            app_class: self.app_class,
            app_name: self.app_name,
            app_version: self.app_version,
            instance_name: self.instance_name,
            runtime: self.runtime,
            supported_ops: self.supported_ops,
            registered_unix_ms: self.registered_unix_ms,
            env,
            name: self.name,
            cwd: self
                .cwd
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            registered: self.registered,
            env_names,
        }
    }
}

fn matches_text(matcher: Option<&TextMatcher>, value: Option<&str>) -> bool {
    matcher.is_none_or(|matcher| value.is_some_and(|value| matcher.is_match(value)))
}

/// Source of unregistered process-table snapshots.
pub trait OsTableProvider: Send + Sync {
    /// Enumerate process identity, name, executable, and cwd.
    ///
    /// Implementations must not read process environments.
    fn enumerate(&self) -> Vec<OsProcess>;
}

/// One process-table row, without environment data.
#[derive(Clone, Debug)]
pub struct OsProcess {
    /// Process id.
    pub pid: u32,
    /// OS start time in Unix milliseconds.
    pub started_at_unix_ms: u64,
    /// Executable basename.
    pub name: String,
    /// Executable path when the OS permits it.
    pub exe: Option<PathBuf>,
    /// Working directory when the OS permits it.
    pub cwd: Option<PathBuf>,
}

#[derive(Debug)]
/// The real OS process table, over `sysinfo`.
pub struct SysinfoProvider;

impl OsTableProvider for SysinfoProvider {
    fn enumerate(&self) -> Vec<OsProcess> {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessRefreshKind::new()
                .with_exe(UpdateKind::Always)
                .with_cwd(UpdateKind::Always),
        );
        system
            .processes()
            .iter()
            .map(|(pid, process)| OsProcess {
                pid: pid.as_u32(),
                started_at_unix_ms: process.start_time().saturating_mul(1000),
                name: process.name().to_owned(),
                exe: process.exe().map(PathBuf::from),
                cwd: process.cwd().map(PathBuf::from),
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct OsCache {
    taken: Option<Instant>,
    rows: Vec<OsProcess>,
}

/// Live query engine with a singleflight, short-lived OS process cache.
pub struct QueryEngine {
    provider: Box<dyn OsTableProvider>,
    cache: Mutex<OsCache>,
    ttl: Duration,
}

impl std::fmt::Debug for QueryEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryEngine")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new(Box::new(SysinfoProvider), OS_TABLE_TTL)
    }
}

impl QueryEngine {
    /// Build an engine with an injectable process provider and cache TTL.
    pub fn new(provider: Box<dyn OsTableProvider>, ttl: Duration) -> Self {
        Self {
            provider,
            cache: Mutex::new(OsCache::default()),
            ttl,
        }
    }

    /// Run one validated query.
    pub fn run(&self, query: &ProcessQuery, registry: &Registry) -> Vec<wire::ProcessInfo> {
        let mut candidates = BTreeMap::<(u32, u64), Candidate>::new();

        for entry in registry.snapshot() {
            if let Some(candidate) = Candidate::from_registration(entry) {
                candidates.insert(
                    (candidate.key.pid, candidate.key.started_at_unix_ms),
                    candidate,
                );
            }
        }

        if query.include_unregistered {
            let rows = self.os_snapshot();
            let registered_pids = candidates
                .keys()
                .map(|(pid, _)| *pid)
                .collect::<BTreeSet<_>>();
            let boot_id = running_process::broker::host_identity::current().boot_id;
            for row in rows {
                // A registrant's start time is OS-derived in milliseconds.
                // PID-only suppression is defense in depth for older clients
                // that sent install time instead: registry data still wins.
                if registered_pids.contains(&row.pid) {
                    continue;
                }
                let candidate = Candidate {
                    key: ProcessKey {
                        pid: row.pid,
                        started_at_unix_ms: row.started_at_unix_ms,
                        boot_id: boot_id.clone(),
                    },
                    name: row.name,
                    exe: row.exe,
                    cwd: row.cwd,
                    registered: false,
                    app_class: String::new(),
                    app_name: String::new(),
                    app_version: String::new(),
                    instance_name: String::new(),
                    runtime: wire::Runtime::Unspecified as i32,
                    supported_ops: Vec::new(),
                    registered_unix_ms: 0,
                    disclosed_env: BTreeMap::new(),
                    disclose_env_names: false,
                };
                candidates
                    .entry((candidate.key.pid, candidate.key.started_at_unix_ms))
                    .or_insert(candidate);
            }
        }

        candidates
            .into_values()
            .filter(|candidate| candidate.matches(query))
            .take(query.limit)
            .map(|candidate| candidate.into_proto(query.include_env))
            .collect()
    }

    fn os_snapshot(&self) -> Vec<OsProcess> {
        let mut cache = self.cache.lock().expect("OS process cache poisoned");
        if cache.taken.is_none_or(|taken| taken.elapsed() >= self.ttl) {
            // Keep the mutex during enumeration: concurrent stale callers
            // share this one refresh instead of stampeding the process table.
            cache.rows = self.provider.enumerate();
            cache.taken = Some(Instant::now());
        }
        cache.rows.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use running_process::broker::server::PeerIdentity;

    use super::*;
    use crate::registry::{AllowPolicy, Disclosure, RegisterRequest};

    const OWNER: &str = "query-owner";

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        rows: Vec<OsProcess>,
    }

    impl OsTableProvider for CountingProvider {
        fn enumerate(&self) -> Vec<OsProcess> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.rows.clone()
        }
    }

    fn wire_query(limit: u32) -> wire::ProcessQuery {
        wire::ProcessQuery {
            limit,
            ..Default::default()
        }
    }

    fn register(
        registry: &Registry,
        pid: u32,
        start: u64,
        exe: &str,
        cwd: Option<&str>,
        env: &[(&str, &str)],
    ) {
        let disclosed_env = env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let key = ProcessKey {
            pid,
            started_at_unix_ms: start,
            boot_id: "boot".into(),
        };
        registry
            .begin_register(
                RegisterRequest {
                    key: key.clone(),
                    exe_path: exe.into(),
                    exe_sha256: [0; 32],
                    app_class: "fixture".into(),
                    app_name: "fixture".into(),
                    app_version: "1".into(),
                    instance_name: String::new(),
                    allow_policy: AllowPolicy {
                        allow_all_ops: true,
                        env_allowlist: env.iter().map(|(key, _)| (*key).to_owned()).collect(),
                    },
                    disclosure: Disclosure {
                        expose_exe_path: true,
                        expose_cmdline: false,
                        expose_env_names: true,
                    },
                    disclosed_cwd: cwd.map(PathBuf::from),
                    disclosed_env,
                    nonce: [pid as u8; 32],
                    supported_ops: vec!["stack_capture".into()],
                    runtime: Runtime::Native,
                    symbol_source: 0,
                    symbol_manifest_path: None,
                    symbol_paths: Vec::new(),
                },
                PeerIdentity {
                    pid,
                    uid_or_sid: OWNER.into(),
                },
                u64::from(pid),
            )
            .unwrap();
        registry.verify_and_arm(&key, true, true).unwrap();
    }

    fn engine(rows: Vec<OsProcess>, calls: Arc<AtomicUsize>) -> QueryEngine {
        QueryEngine::new(
            Box::new(CountingProvider { calls, rows }),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn glob_regex_and_and_semantics_select_the_expected_subset() {
        let registry = Registry::new(OWNER.into());
        register(
            &registry,
            10,
            1000,
            "/bin/clud.exe",
            Some("/work/clud"),
            &[],
        );
        register(
            &registry,
            11,
            1100,
            "/bin/worker-1.exe",
            Some("/srv/worker"),
            &[],
        );
        let engine = engine(Vec::new(), Arc::new(AtomicUsize::new(0)));

        let mut query = wire_query(10);
        query.name_glob = "*clud*.exe".into();
        assert_eq!(
            engine.run(&ProcessQuery::from_proto(query).unwrap(), &registry)[0].name,
            "clud.exe"
        );

        let mut query = wire_query(10);
        query.name_regex = "^worker-\\d+".into();
        assert_eq!(
            engine.run(&ProcessQuery::from_proto(query).unwrap(), &registry)[0].name,
            "worker-1.exe"
        );

        let mut query = wire_query(10);
        query.name_glob = "*.exe".into();
        query.cwd_regex = ".*clud.*".into();
        assert_eq!(
            engine.run(&ProcessQuery::from_proto(query).unwrap(), &registry)[0].name,
            "clud.exe"
        );
    }

    #[test]
    fn env_values_are_allowlisted_and_unregistered_env_is_completely_invisible() {
        let registry = Registry::new(OWNER.into());
        register(&registry, 10, 1000, "/bin/svc", None, &[("FOO", "bar")]);
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = engine(
            vec![OsProcess {
                pid: 20,
                started_at_unix_ms: 2000,
                name: "other".into(),
                exe: None,
                cwd: None,
            }],
            calls,
        );

        let mut query = wire_query(10);
        query.include_unregistered = true;
        query.include_env = true;
        query.env.push(wire::EnvMatch {
            key: "FOO".into(),
            value_exact: Some("bar".into()),
            ..Default::default()
        });
        let matches = engine.run(&ProcessQuery::from_proto(query).unwrap(), &registry);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(matches[0].env_names, ["FOO"]);
        assert!(matches[0].registered);

        let mut query = wire_query(10);
        query.include_unregistered = true;
        query.include_env = true;
        let matches = engine.run(&ProcessQuery::from_proto(query).unwrap(), &registry);
        let unregistered = matches.iter().find(|info| !info.registered).unwrap();
        assert!(unregistered.env.is_empty());
        assert!(unregistered.env_names.is_empty());

        let mut query = wire_query(10);
        query.include_unregistered = true;
        query.env.push(wire::EnvMatch {
            key: "PUBLIC".into(),
            ..Default::default()
        });
        assert!(
            engine
                .run(&ProcessQuery::from_proto(query).unwrap(), &registry)
                .is_empty(),
            "unregistered environment keys must be invisible to filtering"
        );
    }

    #[test]
    fn mandatory_limit_determinism_dedup_and_cache_are_enforced() {
        assert_eq!(
            ProcessQuery::from_proto(wire_query(0)).unwrap_err(),
            QueryError::MissingLimit
        );

        let registry = Registry::new(OWNER.into());
        register(&registry, 2, 2000, "/bin/registered", None, &[]);
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = engine(
            vec![
                OsProcess {
                    pid: 3,
                    started_at_unix_ms: 3000,
                    name: "three".into(),
                    exe: None,
                    cwd: None,
                },
                OsProcess {
                    pid: 2,
                    started_at_unix_ms: 2000,
                    name: "duplicate".into(),
                    exe: None,
                    cwd: None,
                },
                OsProcess {
                    pid: 1,
                    started_at_unix_ms: 1000,
                    name: "one".into(),
                    exe: None,
                    cwd: None,
                },
            ],
            Arc::clone(&calls),
        );
        let mut query = wire_query(2);
        query.include_unregistered = true;
        let query = ProcessQuery::from_proto(query).unwrap();
        let first = engine.run(&query, &registry);
        let second = engine.run(&query, &registry);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(first.len(), 2);
        assert_eq!(first, second);
        assert_eq!(
            first
                .iter()
                .map(|info| info.key.as_ref().unwrap().pid)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(first[1].registered, "registry row must win the duplicate");
    }
}
