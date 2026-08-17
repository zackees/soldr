//! Process re-entrancy guard (soldr#2547 / soldr#2566, first slice).
//!
//! Every Soldr process stamps `IN_SOLDR_PID=<its pid>` into the environment
//! its children inherit. An ordinary Soldr CLI entry that inherits a foreign
//! marker is a `soldr -> tool -> ... -> soldr` re-entry — the class behind
//! months of hang-shaped incidents (a broker probing `soldr --version`,
//! delegation spawn storms, nested daemon launches). Sanctioned re-entries
//! are mechanically distinguishable: compiler-wrapper invocations, multicall
//! shim identities, and internal spawn edges that already carry their own
//! marker variables.
//!
//! Enforcement is opt-in for now: `SOLDR_REENTRANCY_GUARD=strict` turns an
//! unsanctioned re-entry into an immediate diagnostic + exit 1 instead of a
//! silent recursive process tree. soldr's own CI flips the switch
//! (soldr#2566); the default-on rollout is soldr#2547's endgame. Marker
//! stamping happens unconditionally so a strict child can always judge its
//! parentage, whichever mode the parent ran under.

/// Marker every Soldr process exports for its children.
pub const IN_SOLDR_PID_ENV: &str = "IN_SOLDR_PID";

/// Opt-in enforcement switch: `strict` rejects unsanctioned re-entry.
pub const GUARD_MODE_ENV: &str = "SOLDR_REENTRANCY_GUARD";

/// Internal spawn edges that legitimately re-enter Soldr. Each variable is
/// set by exactly one sanctioned producer at its spawn/exec boundary; their
/// presence identifies the edge without argv guessing.
const SANCTIONED_EDGE_ENV_VARS: &[&str] = &[
    "SOLDR_INTERNAL_BROKER_INSTANCE_ID",
    "SOLDR_INTERNAL_DAEMON_EXE",
    "SOLDR_INTERNAL_DAEMON_REEXECED",
    "SOLDR_INTERNAL_INHERIT_PROCESS_GROUP",
    "SOLDR_TRAMPOLINING",
    "SOLDR_GLOBAL_DELEGATING",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardDecision {
    Allow,
    Reject { inherited_pid: u32 },
}

/// Pure decision core, unit-tested exhaustively.
pub(crate) fn decide(
    strict: bool,
    inherited: Option<&str>,
    current_pid: u32,
    shim_identity: bool,
    wrapper_invocation: bool,
    sanctioned_env_present: bool,
) -> GuardDecision {
    if !strict {
        return GuardDecision::Allow;
    }
    let Some(inherited_pid) = inherited.and_then(|value| value.trim().parse::<u32>().ok()) else {
        // Absent or unparseable marker: nothing to judge. An unparseable
        // value is treated as absent rather than hostile — the marker is a
        // cooperative signal, not an auth token.
        return GuardDecision::Allow;
    };
    if inherited_pid == current_pid {
        // Same-PID exec (self-relocation, trampoline re-exec) is the same
        // logical process continuing under a new image.
        return GuardDecision::Allow;
    }
    if shim_identity || wrapper_invocation || sanctioned_env_present {
        return GuardDecision::Allow;
    }
    GuardDecision::Reject { inherited_pid }
}

/// Enforce (when strict) and stamp the marker. Returns `Some(exit_code)`
/// when the process must terminate instead of dispatching.
pub(crate) fn enforce_and_mark(raw_args: &[String]) -> Option<i32> {
    let strict = std::env::var(GUARD_MODE_ENV)
        .map(|value| value.trim().eq_ignore_ascii_case("strict"))
        .unwrap_or(false);
    let inherited = std::env::var(IN_SOLDR_PID_ENV).ok();
    let current_pid = std::process::id();
    let shim_identity = raw_args
        .first()
        .is_some_and(|argv0| crate::multicall::is_shim_identity(argv0));
    let wrapper_invocation = raw_args
        .get(1)
        .is_some_and(|arg| crate::wrapper::is_wrapper_invocation(arg));
    let sanctioned_env_present = SANCTIONED_EDGE_ENV_VARS
        .iter()
        .any(|name| std::env::var_os(name).is_some());

    let decision = decide(
        strict,
        inherited.as_deref(),
        current_pid,
        shim_identity,
        wrapper_invocation,
        sanctioned_env_present,
    );

    if let GuardDecision::Reject { inherited_pid } = decision {
        emit_rejection(inherited_pid, current_pid, raw_args);
        return Some(1);
    }

    // Stamp unconditionally, after judgment: children inherit OUR pid, and
    // a wrapper-shaped entry refreshes ownership to itself exactly as
    // soldr#2547's design requires.
    std::env::set_var(IN_SOLDR_PID_ENV, current_pid.to_string());
    None
}

/// Routing variables the diagnostic is allowed to disclose. An allowlist,
/// not a denylist: the guard fires on process graphs that may carry
/// credentials, registry tokens, or signing material elsewhere in the
/// environment, and soldr#2547 item 5 requires everything unrelated to how
/// this process came to exist to stay out of the record.
const DISCLOSED_ROUTING_ENV_VARS: &[&str] = &[
    "RUSTC_WRAPPER",
    "SOLDR_RUSTC_WRAPPER",
    "SOLDR_GLOBAL_DELEGATING",
    "SOLDR_TRAMPOLINING",
];

/// How many argv entries the record keeps. Bounded per soldr#2547 item 5;
/// a rejected re-entry is identified by its head, and a cargo-shaped argv
/// can run to hundreds of entries.
const RECORDED_ARGV_LIMIT: usize = 16;

/// Bounded diagnostic: both processes, the argv head, and only the routing
/// variables relevant to how this process came to exist — never the full
/// environment (soldr#2547 item 5).
fn emit_rejection(inherited_pid: u32, current_pid: u32, raw_args: &[String]) {
    let argv_head = raw_args
        .iter()
        .take(4)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    eprintln!(
        "soldr: rejected unsanctioned Soldr re-entrancy ({GUARD_MODE_ENV}=strict):\n\
         soldr:   inherited {IN_SOLDR_PID_ENV}={inherited_pid}, this pid={current_pid}\n\
         soldr:   argv: {argv_head}\n\
         soldr:   exe: {exe}\n\
         soldr:   cwd: {cwd}"
    );
    for name in DISCLOSED_ROUTING_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            eprintln!("soldr:   {name}={value}");
        }
    }
    eprintln!(
        "soldr: a soldr -> tool -> ... -> soldr chain reached an ordinary CLI entry; \
         this looks like a hang in the wild and is forbidden under strict mode \
         (soldr#2547, soldr#2566)"
    );
    // soldr#2547 item 5: the diagnostic must survive stderr being redirected
    // or absent. The processes this guard exists to catch are precisely the
    // ones nobody is watching — a detached `broker serve` writes its stderr
    // into a spawn log at best, and a rejected child of a tool chain writes
    // it wherever that tool decided. Persist the same facts where `soldr
    // logs` can find them afterwards.
    match write_rejection_record(inherited_pid, current_pid, raw_args) {
        Some(path) => eprintln!("soldr:   record: {}", path.display()),
        // Deliberately quiet: a rejection that cannot be journalled is still
        // a rejection, and the stderr text above already carries every field.
        None => eprintln!("soldr:   record: <not written>"),
    }
}

/// Persist the rejection as JSON under `<soldr root>/logs/reentrancy/`,
/// mirroring the `debug-trace` layout. Best effort in every step: this runs
/// on a path that is already exiting 1, so a read-only disk or an
/// unresolvable home must not turn a clean refusal into a crash.
fn write_rejection_record(
    inherited_pid: u32,
    current_pid: u32,
    raw_args: &[String],
) -> Option<std::path::PathBuf> {
    let dir = crate::core::SoldrPaths::new()
        .ok()?
        .root
        .join("logs")
        .join("reentrancy");
    std::fs::create_dir_all(&dir).ok()?;
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{unix_ms}-{current_pid}.json"));
    let record = render_rejection_record(
        inherited_pid,
        current_pid,
        raw_args,
        unix_ms,
        std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .ok(),
        std::env::current_dir()
            .map(|cwd| cwd.display().to_string())
            .ok(),
        &disclosed_routing_env(),
    );
    std::fs::write(&path, record).ok()?;
    Some(path)
}

/// The routing variables actually present, in allowlist order.
fn disclosed_routing_env() -> Vec<(String, String)> {
    DISCLOSED_ROUTING_ENV_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect()
}

/// Render the record. Pure so the schema and — more importantly — the
/// redaction are unit-testable without touching the process environment or
/// the filesystem.
fn render_rejection_record(
    inherited_pid: u32,
    current_pid: u32,
    raw_args: &[String],
    unix_ms: u128,
    exe: Option<String>,
    cwd: Option<String>,
    routing_env: &[(String, String)],
) -> String {
    let argv: Vec<&String> = raw_args.iter().take(RECORDED_ARGV_LIMIT).collect();
    let routing: serde_json::Map<String, serde_json::Value> = routing_env
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect();
    serde_json::json!({
        "schema_version": REJECTION_SCHEMA_VERSION,
        "event": "reentrancy_rejected",
        "unix_ms": unix_ms as u64,
        "inherited_in_soldr_pid": inherited_pid,
        "pid": current_pid,
        "exe": exe,
        "cwd": cwd,
        "argv": argv,
        "argv_truncated": raw_args.len() > RECORDED_ARGV_LIMIT,
        "routing_env": routing,
    })
    .to_string()
}

/// Stable shape for consumers of `logs/reentrancy/*.json`.
///
/// Version 1 omits the process *role* and the immediate parent pid that
/// soldr#2547 item 5 also asks for: roles are the issue's next slice (the
/// guard currently judges a flat sanctioned-edge allowlist, not a role
/// matrix), and a parent pid needs a new cross-platform primitive in
/// `soldr-platform` — Windows has no std equivalent of `getppid`. Both are
/// additive fields when they land.
const REJECTION_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_strict_always_allows() {
        assert_eq!(
            decide(false, Some("123"), 456, false, false, false),
            GuardDecision::Allow
        );
    }

    #[test]
    fn strict_without_marker_allows() {
        assert_eq!(
            decide(true, None, 456, false, false, false),
            GuardDecision::Allow
        );
    }

    #[test]
    fn strict_with_unparseable_marker_allows() {
        assert_eq!(
            decide(true, Some("not-a-pid"), 456, false, false, false),
            GuardDecision::Allow
        );
    }

    #[test]
    fn same_pid_exec_is_not_reentry() {
        assert_eq!(
            decide(true, Some("456"), 456, false, false, false),
            GuardDecision::Allow
        );
    }

    #[test]
    fn foreign_marker_on_plain_cli_entry_is_rejected() {
        assert_eq!(
            decide(true, Some("123"), 456, false, false, false),
            GuardDecision::Reject { inherited_pid: 123 }
        );
    }

    fn sample_record(raw_args: &[String], routing: &[(String, String)]) -> serde_json::Value {
        let rendered = render_rejection_record(
            123,
            456,
            raw_args,
            1_700_000_000_000,
            Some("/opt/soldr".to_string()),
            Some("/work".to_string()),
            routing,
        );
        serde_json::from_str(&rendered).expect("record must be valid JSON")
    }

    #[test]
    fn record_carries_both_processes_and_the_invocation() {
        let argv = vec!["soldr".to_string(), "--version".to_string()];
        let record = sample_record(&argv, &[]);
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["event"], "reentrancy_rejected");
        assert_eq!(record["inherited_in_soldr_pid"], 123);
        assert_eq!(record["pid"], 456);
        assert_eq!(record["unix_ms"], 1_700_000_000_000u64);
        assert_eq!(record["exe"], "/opt/soldr");
        assert_eq!(record["cwd"], "/work");
        assert_eq!(record["argv"][1], "--version");
        assert_eq!(record["argv_truncated"], false);
    }

    #[test]
    fn record_discloses_only_routing_variables() {
        // The whole point of the allowlist: this guard fires on process
        // graphs that may carry tokens elsewhere in the environment, and the
        // record is written to disk where it outlives the process.
        let routing = vec![
            ("RUSTC_WRAPPER".to_string(), "/opt/soldr".to_string()),
            ("SOLDR_TRAMPOLINING".to_string(), "1".to_string()),
        ];
        let record = sample_record(&[], &routing);
        assert_eq!(record["routing_env"]["RUSTC_WRAPPER"], "/opt/soldr");
        assert_eq!(record["routing_env"]["SOLDR_TRAMPOLINING"], "1");
        assert_eq!(
            record["routing_env"].as_object().expect("object").len(),
            2,
            "only the variables handed in may appear"
        );
        assert!(
            DISCLOSED_ROUTING_ENV_VARS.len() == 4
                && !DISCLOSED_ROUTING_ENV_VARS.contains(&"PATH")
                && !DISCLOSED_ROUTING_ENV_VARS.contains(&"HOME"),
            "the disclosure allowlist must stay narrow: {DISCLOSED_ROUTING_ENV_VARS:?}"
        );
    }

    #[test]
    fn long_argv_is_bounded_and_says_so() {
        let argv: Vec<String> = (0..40).map(|index| format!("--arg{index}")).collect();
        let record = sample_record(&argv, &[]);
        assert_eq!(
            record["argv"].as_array().expect("array").len(),
            RECORDED_ARGV_LIMIT
        );
        assert_eq!(record["argv_truncated"], true);
    }

    #[test]
    fn a_missing_exe_or_cwd_records_null_rather_than_failing() {
        let rendered = render_rejection_record(1, 2, &[], 0, None, None, &[]);
        let record: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert!(record["exe"].is_null());
        assert!(record["cwd"].is_null());
    }

    #[test]
    fn each_sanctioned_edge_passes() {
        for (shim, wrapper, env) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert_eq!(
                decide(true, Some("123"), 456, shim, wrapper, env),
                GuardDecision::Allow,
                "edge (shim={shim}, wrapper={wrapper}, env={env}) must be sanctioned"
            );
        }
    }
}
