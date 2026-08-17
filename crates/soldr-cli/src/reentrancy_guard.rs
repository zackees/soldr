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

/// Role this process exports for its children, beside [`IN_SOLDR_PID_ENV`].
///
/// soldr#2547 item 2: an edge variable says *how* a child was spawned but not
/// *who* spawned it, and item 4's matrix needs both.
pub const IN_SOLDR_ROLE_ENV: &str = "IN_SOLDR_ROLE";

/// The edge a user-facing global-version probe/delegation carries.
const GLOBAL_DELEGATION_EDGE: &str = "SOLDR_GLOBAL_DELEGATING";

/// Internal spawn edges that legitimately re-enter Soldr. Each variable is
/// set by exactly one sanctioned producer at its spawn/exec boundary; their
/// presence identifies the edge without argv guessing.
const SANCTIONED_EDGE_ENV_VARS: &[&str] = &[
    "SOLDR_INTERNAL_BROKER_INSTANCE_ID",
    "SOLDR_INTERNAL_DAEMON_EXE",
    "SOLDR_INTERNAL_DAEMON_REEXECED",
    "SOLDR_INTERNAL_INHERIT_PROCESS_GROUP",
    "SOLDR_TRAMPOLINING",
    GLOBAL_DELEGATION_EDGE,
];

/// What kind of Soldr process this is, as far as the guard needs to care.
///
/// Only the roles this entry point can actually *be*: `soldr-daemon` is a
/// different binary and never reaches `enforce_and_mark`, so inventing a
/// `Daemon` variant here would be a label nothing could ever set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    FrontDoor,
    Wrapper,
    Broker,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Role::FrontDoor => "front-door",
            Role::Wrapper => "wrapper",
            Role::Broker => "broker",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "front-door" => Some(Role::FrontDoor),
            "wrapper" => Some(Role::Wrapper),
            "broker" => Some(Role::Broker),
            _ => None,
        }
    }
}

/// This process's own role, from the shape of its invocation.
pub(crate) fn own_role(raw_args: &[String], wrapper_invocation: bool) -> Role {
    if wrapper_invocation {
        return Role::Wrapper;
    }
    if raw_args.get(1).is_some_and(|arg| arg == "broker") {
        return Role::Broker;
    }
    Role::FrontDoor
}

/// soldr#2547 item 4's matrix, as a deny-list of one.
///
/// Written this way on purpose. The guard runs strict in soldr's own CI, so a
/// matrix that enumerated what is *allowed* would reject every edge nobody
/// thought to list — turning an unfinished table into a broad outage. Every
/// pair keeps today's behaviour except the one the incident named.
///
/// That pair: a **broker** must never authorize a user-facing global-version
/// probe. `probe_version` spawns `<global soldr> --version` carrying
/// `SOLDR_GLOBAL_DELEGATING`, which the flat allowlist accepted from anyone —
/// so `soldr-broker broker serve -> soldr --version`, the exact shape that
/// consumed a core for 17 s and pushed broker readiness past its deadline,
/// was *sanctioned*. A top-level front door probing the global version stays
/// legitimate, which is why the rule keys on the parent's role rather than on
/// the edge alone.
///
/// `is_delegation_exempt` already stops a broker from reaching the probe, so
/// this is the structural backstop for that policy, not its only enforcement.
fn edge_permitted(parent_role: Option<Role>, edge: &str) -> bool {
    !(parent_role == Some(Role::Broker) && edge == GLOBAL_DELEGATION_EDGE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardDecision {
    Allow,
    Reject {
        inherited_pid: u32,
        reason: RejectReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectReason {
    /// An ordinary CLI entry inherited a foreign marker with no sanctioned
    /// edge at all.
    UnsanctionedEntry,
    /// A sanctioned edge was present, but not one this parent role may
    /// authorize (soldr#2547 item 4).
    ForbiddenRoleTransition,
}

impl RejectReason {
    fn as_str(self) -> &'static str {
        match self {
            RejectReason::UnsanctionedEntry => "unsanctioned-entry",
            RejectReason::ForbiddenRoleTransition => "forbidden-role-transition",
        }
    }

    fn explain(self) -> &'static str {
        match self {
            RejectReason::UnsanctionedEntry => {
                "a soldr -> tool -> ... -> soldr chain reached an ordinary CLI entry; \
                 this looks like a hang in the wild and is forbidden under strict mode \
                 (soldr#2547, soldr#2566)"
            }
            RejectReason::ForbiddenRoleTransition => {
                "the spawning Soldr's role may not authorize this transition: a broker \
                 must never launch a user-facing global-version probe (soldr#2547 item 4) \
                 -- that is the 17s core-burning re-entry this guard exists for"
            }
        }
    }
}

/// Pure decision core, unit-tested exhaustively.
pub(crate) fn decide(
    strict: bool,
    inherited: Option<&str>,
    current_pid: u32,
    shim_identity: bool,
    wrapper_invocation: bool,
    sanctioned_edges: &[&str],
    parent_role: Option<Role>,
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
    if shim_identity || wrapper_invocation {
        return GuardDecision::Allow;
    }
    if sanctioned_edges.is_empty() {
        return GuardDecision::Reject {
            inherited_pid,
            reason: RejectReason::UnsanctionedEntry,
        };
    }
    // Any one permitted edge authorizes the entry, matching the flat
    // allowlist this replaced. What changed is that an edge the parent's
    // role may not authorize no longer counts as one.
    if sanctioned_edges
        .iter()
        .any(|edge| edge_permitted(parent_role, edge))
    {
        return GuardDecision::Allow;
    }
    GuardDecision::Reject {
        inherited_pid,
        reason: RejectReason::ForbiddenRoleTransition,
    }
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
    let sanctioned_edges: Vec<&str> = SANCTIONED_EDGE_ENV_VARS
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    let parent_role = std::env::var(IN_SOLDR_ROLE_ENV)
        .ok()
        .and_then(|value| Role::parse(&value));

    let decision = decide(
        strict,
        inherited.as_deref(),
        current_pid,
        shim_identity,
        wrapper_invocation,
        &sanctioned_edges,
        parent_role,
    );

    if let GuardDecision::Reject {
        inherited_pid,
        reason,
    } = decision
    {
        emit_rejection(inherited_pid, current_pid, raw_args, reason, parent_role);
        return Some(1);
    }

    // Stamp unconditionally, after judgment: children inherit OUR pid, and
    // a wrapper-shaped entry refreshes ownership to itself exactly as
    // soldr#2547's design requires. The role goes with it, so a child can
    // judge *who* spawned it and not merely *how* (item 4's matrix).
    std::env::set_var(IN_SOLDR_PID_ENV, current_pid.to_string());
    std::env::set_var(
        IN_SOLDR_ROLE_ENV,
        own_role(raw_args, wrapper_invocation).as_str(),
    );
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
fn emit_rejection(
    inherited_pid: u32,
    current_pid: u32,
    raw_args: &[String],
    reason: RejectReason,
    parent_role: Option<Role>,
) {
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
         soldr:   reason: {reason}\n\
         soldr:   inherited {IN_SOLDR_PID_ENV}={inherited_pid} (role {role}), this pid={current_pid}\n\
         soldr:   argv: {argv_head}\n\
         soldr:   exe: {exe}\n\
         soldr:   cwd: {cwd}",
        reason = reason.as_str(),
        role = parent_role.map_or("unknown", Role::as_str),
    );
    for name in DISCLOSED_ROUTING_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            eprintln!("soldr:   {name}={value}");
        }
    }
    eprintln!("soldr: {}", reason.explain());
    // soldr#2547 item 5: the diagnostic must survive stderr being redirected
    // or absent. The processes this guard exists to catch are precisely the
    // ones nobody is watching — a detached `broker serve` writes its stderr
    // into a spawn log at best, and a rejected child of a tool chain writes
    // it wherever that tool decided. Persist the same facts where `soldr
    // logs` can find them afterwards.
    match write_rejection_record(inherited_pid, current_pid, raw_args, reason, parent_role) {
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
    reason: RejectReason,
    parent_role: Option<Role>,
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
    let record = render_rejection_record(&RejectionRecord {
        inherited_pid,
        current_pid,
        raw_args,
        unix_ms,
        exe: std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .ok(),
        cwd: std::env::current_dir()
            .map(|cwd| cwd.display().to_string())
            .ok(),
        routing_env: &disclosed_routing_env(),
        reason,
        parent_role,
    });
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
/// Everything one rejection record reports. A struct rather than nine
/// positional arguments: the two pids and the two roles are all easy to
/// transpose at a call site, and a swapped pair here would misattribute the
/// incident the record exists to explain.
struct RejectionRecord<'a> {
    inherited_pid: u32,
    current_pid: u32,
    raw_args: &'a [String],
    unix_ms: u128,
    exe: Option<String>,
    cwd: Option<String>,
    routing_env: &'a [(String, String)],
    reason: RejectReason,
    parent_role: Option<Role>,
}

fn render_rejection_record(record: &RejectionRecord<'_>) -> String {
    let argv: Vec<&String> = record.raw_args.iter().take(RECORDED_ARGV_LIMIT).collect();
    let routing: serde_json::Map<String, serde_json::Value> = record
        .routing_env
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect();
    serde_json::json!({
        "schema_version": REJECTION_SCHEMA_VERSION,
        "event": "reentrancy_rejected",
        "unix_ms": record.unix_ms as u64,
        "reason": record.reason.as_str(),
        "inherited_in_soldr_pid": record.inherited_pid,
        "inherited_role": record.parent_role.map(Role::as_str),
        "pid": record.current_pid,
        "exe": record.exe,
        "cwd": record.cwd,
        "argv": argv,
        "argv_truncated": record.raw_args.len() > RECORDED_ARGV_LIMIT,
        "routing_env": routing,
    })
    .to_string()
}

/// Stable shape for consumers of `logs/reentrancy/*.json`.
///
/// Version 1 carries the spawning process's role and the rejection reason as
/// of soldr#2547 item 4; both are additive fields on an unreleased schema, so
/// the version does not move. Still absent is the immediate parent pid, which
/// needs a new cross-platform primitive in `soldr-platform` — Windows has no
/// std `getppid`.
///
const REJECTION_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_strict_always_allows() {
        assert_eq!(
            decide(false, Some("123"), 456, false, false, &[], None),
            GuardDecision::Allow
        );
    }

    #[test]
    fn strict_without_marker_allows() {
        assert_eq!(
            decide(true, None, 456, false, false, &[], None),
            GuardDecision::Allow
        );
    }

    #[test]
    fn strict_with_unparseable_marker_allows() {
        assert_eq!(
            decide(true, Some("not-a-pid"), 456, false, false, &[], None),
            GuardDecision::Allow
        );
    }

    #[test]
    fn same_pid_exec_is_not_reentry() {
        assert_eq!(
            decide(true, Some("456"), 456, false, false, &[], None),
            GuardDecision::Allow
        );
    }

    #[test]
    fn foreign_marker_on_plain_cli_entry_is_rejected() {
        assert_eq!(
            decide(true, Some("123"), 456, false, false, &[], None),
            GuardDecision::Reject {
                inherited_pid: 123,
                reason: RejectReason::UnsanctionedEntry
            }
        );
    }

    fn sample_record(raw_args: &[String], routing: &[(String, String)]) -> serde_json::Value {
        let rendered = render_rejection_record(&RejectionRecord {
            inherited_pid: 123,
            current_pid: 456,
            raw_args,
            unix_ms: 1_700_000_000_000,
            exe: Some("/opt/soldr".to_string()),
            cwd: Some("/work".to_string()),
            routing_env: routing,
            reason: RejectReason::UnsanctionedEntry,
            parent_role: Some(Role::FrontDoor),
        });
        serde_json::from_str(&rendered).expect("record must be valid JSON")
    }

    /// soldr#2547 item 4's regression test: the shape that motivated the
    /// whole guard.
    ///
    /// `issue_2481_...` staged an incumbent image and launched it as `soldr
    /// broker serve`. Before binding its endpoint that broker entered the
    /// user-facing `prefer_newer_global` policy and spawned `<global soldr>
    /// --version`, which burned a core for 17 s and pushed readiness past the
    /// deadline. `probe_version` sets `SOLDR_GLOBAL_DELEGATING` on that
    /// child, so the flat allowlist called it sanctioned.
    #[test]
    fn a_broker_may_not_authorize_a_global_version_probe() {
        assert_eq!(
            decide(
                true,
                Some("123"),
                456,
                false,
                false,
                &[GLOBAL_DELEGATION_EDGE],
                Some(Role::Broker),
            ),
            GuardDecision::Reject {
                inherited_pid: 123,
                reason: RejectReason::ForbiddenRoleTransition,
            }
        );
    }

    #[test]
    fn a_front_door_may_still_probe_the_global_version() {
        // The legitimate counterpart, and the reason the rule keys on the
        // parent's role instead of banning the edge outright.
        for parent in [Some(Role::FrontDoor), Some(Role::Wrapper), None] {
            assert_eq!(
                decide(
                    true,
                    Some("123"),
                    456,
                    false,
                    false,
                    &[GLOBAL_DELEGATION_EDGE],
                    parent,
                ),
                GuardDecision::Allow,
                "{parent:?} must keep its probe"
            );
        }
    }

    #[test]
    fn a_broker_may_still_launch_its_daemon() {
        // The matrix must not break what the broker legitimately does: item 4
        // says it "may launch/adopt its daemon roles".
        for edge in [
            "SOLDR_INTERNAL_DAEMON_EXE",
            "SOLDR_INTERNAL_DAEMON_REEXECED",
            "SOLDR_INTERNAL_BROKER_INSTANCE_ID",
            "SOLDR_INTERNAL_INHERIT_PROCESS_GROUP",
            "SOLDR_TRAMPOLINING",
        ] {
            assert_eq!(
                decide(
                    true,
                    Some("123"),
                    456,
                    false,
                    false,
                    &[edge],
                    Some(Role::Broker)
                ),
                GuardDecision::Allow,
                "a broker must keep authorizing {edge}"
            );
        }
    }

    #[test]
    fn a_permitted_edge_alongside_a_forbidden_one_still_authorizes() {
        // Matches the flat allowlist it replaced: any one sanctioned edge is
        // enough. A daemon launch that also happens to carry the delegation
        // marker is a daemon launch.
        assert_eq!(
            decide(
                true,
                Some("123"),
                456,
                false,
                false,
                &[GLOBAL_DELEGATION_EDGE, "SOLDR_INTERNAL_DAEMON_EXE"],
                Some(Role::Broker),
            ),
            GuardDecision::Allow
        );
    }

    #[test]
    fn own_role_reads_the_invocation_shape() {
        let argv = |args: &[&str]| -> Vec<String> {
            args.iter().map(|value| (*value).to_string()).collect()
        };
        assert_eq!(
            own_role(&argv(&["soldr", "broker", "serve"]), false),
            Role::Broker
        );
        assert_eq!(
            own_role(&argv(&["soldr", "status"]), false),
            Role::FrontDoor
        );
        assert_eq!(
            own_role(&argv(&["soldr", "/path/rustc"]), true),
            Role::Wrapper
        );
        // A wrapper re-entry wins over the positional check, because the
        // compiler path can be anything at all.
        assert_eq!(own_role(&argv(&["soldr", "broker"]), true), Role::Wrapper);
    }

    #[test]
    fn roles_round_trip_through_the_environment_value() {
        for role in [Role::FrontDoor, Role::Wrapper, Role::Broker] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("nonsense"), None);
        // An unknown role is `None`, i.e. unconstrained — a newer soldr
        // stamping a role this build has never heard of must not have its
        // children rejected.
        assert_eq!(
            decide(
                true,
                Some("123"),
                456,
                false,
                false,
                &[GLOBAL_DELEGATION_EDGE],
                None
            ),
            GuardDecision::Allow
        );
    }

    #[test]
    fn the_record_names_the_reason_and_the_spawning_role() {
        let rendered = render_rejection_record(&RejectionRecord {
            inherited_pid: 123,
            current_pid: 456,
            raw_args: &[],
            unix_ms: 0,
            exe: None,
            cwd: None,
            routing_env: &[],
            reason: RejectReason::ForbiddenRoleTransition,
            parent_role: Some(Role::Broker),
        });
        let record: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
        assert_eq!(record["reason"], "forbidden-role-transition");
        assert_eq!(record["inherited_role"], "broker");
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
        let rendered = render_rejection_record(&RejectionRecord {
            inherited_pid: 1,
            current_pid: 2,
            raw_args: &[],
            unix_ms: 0,
            exe: None,
            cwd: None,
            routing_env: &[],
            reason: RejectReason::UnsanctionedEntry,
            parent_role: None,
        });
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
                decide(
                    true,
                    Some("123"),
                    456,
                    shim,
                    wrapper,
                    if env { &["SOLDR_TRAMPOLINING"] } else { &[] },
                    None
                ),
                GuardDecision::Allow,
                "edge (shim={shim}, wrapper={wrapper}, env={env}) must be sanctioned"
            );
        }
    }
}
