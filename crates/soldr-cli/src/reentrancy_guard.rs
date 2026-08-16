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
    for name in [
        "RUSTC_WRAPPER",
        "SOLDR_RUSTC_WRAPPER",
        "SOLDR_GLOBAL_DELEGATING",
        "SOLDR_TRAMPOLINING",
    ] {
        if let Ok(value) = std::env::var(name) {
            eprintln!("soldr:   {name}={value}");
        }
    }
    eprintln!(
        "soldr: a soldr -> tool -> ... -> soldr chain reached an ordinary CLI entry; \
         this looks like a hang in the wild and is forbidden under strict mode \
         (soldr#2547, soldr#2566)"
    );
}

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
