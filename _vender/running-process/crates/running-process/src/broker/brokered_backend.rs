//! Structurally-enforced fast-bind contract for v2 brokered daemons (#497).
//!
//! ## Why
//!
//! The v1 launcher (`BackendLauncher::probe_with_service`) requires the
//! spawned daemon to answer an IPC probe within
//! `DEFAULT_ENDPOINT_PROBE_TIMEOUT` (~500 ms) after spawn. That budget is
//! hard-coded in the launcher; nothing in the type system prevents a
//! daemon implementer from doing 3 s of state loading inside its
//! bootstrap before the IPC endpoint becomes probe-able. zccache#640
//! and zccache#784 fix this consumer-side; #497 lifts the invariant
//! into a broker-owned contract so future brokered services (fbuild
//! daemon, soldr cache-daemon) cannot silently regress.
//!
//! ## Shape (Option A from #497)
//!
//! ```text
//! bind(&endpoint) -> IpcListener        // SYNC, microseconds, no state access
//!     |
//!     v
//! write_lock_file                       // broker-orchestrated
//!     |
//!     v
//! serve(listener) -> !                  // free to spawn_blocking, take 30s warming
//! ```
//!
//! `bind` has no access to `State` — the daemon physically cannot do
//! disk I/O before the endpoint is up. The fast-bind property becomes
//! a compile-time consequence of the trait shape.
//!
//! Option B (broker-owned bind via inherited file descriptors / named-
//! pipe handles) is a strictly stronger refinement deferred to a
//! follow-up; this slice lands Option A as the minimum viable trait
//! shape so downstream conformance tests + daemon migrations have a
//! stable target.

use std::error::Error;
use std::fmt;

/// Reasons a [`BrokeredBackend::bind`] call can fail.
///
/// Deliberately small. The broker treats every variant identically
/// (declare the spawn dead, surface the error to the operator); the
/// taxonomy exists so daemons can produce useful logs without inventing
/// their own error types.
#[derive(Debug)]
pub enum BindError {
    /// The endpoint string was not a valid platform path / pipe name.
    InvalidEndpoint(String),

    /// Another process already holds the endpoint (`EADDRINUSE`,
    /// `ERROR_PIPE_BUSY`, etc.).
    AlreadyBound(String),

    /// Underlying OS error from the bind syscall.
    Io(std::io::Error),

    /// Catch-all for daemon-specific bind failures (permission
    /// denied via custom security policy, etc.).
    Other(String),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint(s) => write!(f, "invalid endpoint: {s}"),
            Self::AlreadyBound(s) => write!(f, "endpoint already bound: {s}"),
            Self::Io(e) => write!(f, "bind io error: {e}"),
            Self::Other(s) => write!(f, "bind error: {s}"),
        }
    }
}

impl Error for BindError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BindError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Uninhabited type used as the return type of [`BrokeredBackend::serve`]
/// and [`bootstrap`] to express "this function never returns" on
/// stable Rust.
///
/// `!` (the bare never type) is nightly-only as a return-position type;
/// an empty enum has the same uninhabitedness guarantee and compiles
/// on stable. Implementers cannot construct one — the only way to
/// satisfy the signature is to diverge (loop, panic, exit).
#[derive(Debug)]
pub enum Never {}

/// Cross-platform `IpcListener` placeholder.
///
/// Until the v2 broker baseline lands a unified listener wrapper
/// (subsequent slice of #488), this type alias points at the existing
/// `interprocess::local_socket::Listener` so daemons can start
/// implementing [`BrokeredBackend`] today. A typedef shift later
/// won't break implementers since they construct the listener via
/// platform-neutral helpers, not by naming the type.
pub type IpcListener = interprocess::local_socket::Listener;

/// Opaque endpoint identifier the broker hands the daemon's [`bind`]
/// method. Today a plain string (matches `ServiceDefinition`'s endpoint
/// field shape); will gain structure as the v2 broker baseline grows.
///
/// [`bind`]: BrokeredBackend::bind
pub type Endpoint = str;

/// The fast-bind contract a v2 brokered daemon implements.
///
/// Trait method ordering matches the orchestration the broker runs
/// inside [`bootstrap`]:
///
/// 1. [`bind`](BrokeredBackend::bind) — claim the kernel resource.
///    Synchronous, expected to complete in microseconds. **Takes only
///    the endpoint** — no `&mut self`, no associated-`State` parameter
///    — so it is structurally impossible to perform daemon-state
///    initialization before this returns.
/// 2. (broker-orchestrated) write the lockfile + report spawn success
///    to the operator.
/// 3. [`serve`](BrokeredBackend::serve) — accept connections forever.
///    Free to `spawn_blocking` for arbitrarily slow state loads; the
///    broker does not observe this. Clients connecting during the
///    warmup window queue in the OS accept backlog or see whatever
///    cold-path semantics the daemon defines.
pub trait BrokeredBackend {
    /// Daemon-specific state that survives between requests.
    ///
    /// Allocated and consumed inside `serve` — never visible to
    /// `bind`. The trait's structural guarantee is precisely that
    /// state initialization cannot run before the endpoint is bound.
    type State: Send + 'static;

    /// Bind the IPC listener. **No state access, no disk I/O.**
    ///
    /// The broker enforces hang detection by failing the spawn if this
    /// does not return (or if the resulting listener is not probe-able)
    /// within `DEFAULT_ENDPOINT_PROBE_TIMEOUT`.
    fn bind(endpoint: &Endpoint) -> Result<IpcListener, BindError>;

    /// Serve forever on the bound listener.
    ///
    /// Free to initialize `State` synchronously, `spawn_blocking` heavy
    /// loads, or anything else — the broker has already declared the
    /// daemon "spawned successfully" by this point.
    ///
    /// Returns `!` because a brokered daemon's normal control flow is
    /// to serve until termination signal; clean shutdown is via process
    /// exit. Implementers that want graceful shutdown plumb it through
    /// `State` (e.g. an `AtomicBool` checked between accept loops).
    fn serve(listener: IpcListener) -> Never;
}

/// Run the broker-side fast-bind orchestration for a `BrokeredBackend`.
///
/// 1. Call `B::bind(endpoint)`. Failure → propagate the [`BindError`].
/// 2. Hand the listener to `B::serve`, which never returns.
///
/// The function's signature is `Result<(), BindError>` rather than
/// `Result<Never, …>` so callers don't have to spell `Never` in their
/// return type just to call `bootstrap`. The body still proves
/// divergence: `B::serve(listener)` returns the uninhabited [`Never`],
/// which coerces to `()` via Rust's never-type coercion — control
/// flow that reaches the end of the body would require constructing
/// a `Never` value, which the type system forbids.
///
/// In the full v2 baseline, the broker calls this from inside the
/// spawned daemon's `main()`. Slice 3c–4 of #488 has the v2 broker
/// scaffold but does not yet exercise this trait; this function is the
/// integration seam future slices will use.
// `match B::serve(listener) {}` is the documented pattern for proving
// divergence via an uninhabited type, but rustc's `unreachable_code`
// lint still flags the match itself because flow analysis types
// `B::serve` as `Never`. The lint help text recommends precisely this
// `#[allow]` for the case.
#[allow(unreachable_code)]
pub fn bootstrap<B: BrokeredBackend>(endpoint: &Endpoint) -> Result<(), BindError> {
    // Broker-owned bind (#500 slice 32): when the broker bound the endpoint
    // and handed the listener over, adopt it instead of binding a second
    // one. Binding again would fail with `AlreadyBound` against the broker's
    // own listener — the two paths cannot both claim the endpoint.
    //
    // Today nothing sets that variable, so this is `Ok(None)` and the daemon
    // binds for itself exactly as before. The launcher half lands separately;
    // this side has to exist first, or enabling it there would break every
    // launch at once.
    let listener = match crate::broker::broker_owned_bind::recover_from_env() {
        Ok(Some(inherited)) => inherited,
        Ok(None) => B::bind(endpoint)?,
        // A descriptor was advertised and could not be adopted. Falling back
        // to `B::bind` here would leave the broker holding a listener nobody
        // serves while this process binds a second endpoint — a daemon that
        // looks healthy and is unreachable. Failing closed is the honest
        // outcome.
        Err(error) => return Err(BindError::Io(error)),
    };
    // Lockfile write lands in the v2 broker baseline; once it does,
    // it goes here, between `bind` and `serve`.
    match B::serve(listener) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;

    /// Reference implementation used to verify the trait shape compiles.
    struct StubBackend;

    impl BrokeredBackend for StubBackend {
        type State = ();

        fn bind(endpoint: &Endpoint) -> Result<IpcListener, BindError> {
            // Bind a real platform listener at a unique test name so the
            // path exercises the actual interprocess API surface, not
            // just trait dispatch. The caller passes a per-test
            // suffix via `endpoint` so parallel cargo-test runs don't
            // collide on the same name.
            #[cfg(windows)]
            let name = {
                use interprocess::local_socket::{GenericNamespaced, ToNsName};
                let bare = format!("rp-brokered-backend-stub-{endpoint}");
                ToNsName::to_ns_name::<GenericNamespaced>(bare.as_str())?.into_owned()
            };
            #[cfg(unix)]
            let name = {
                use interprocess::local_socket::{GenericFilePath, ToFsName};
                let path =
                    std::env::temp_dir().join(format!("rp-brokered-backend-stub-{endpoint}.sock"));
                let _ = std::fs::remove_file(&path);
                ToFsName::to_fs_name::<GenericFilePath>(path.to_string_lossy().as_ref())?
                    .into_owned()
            };
            let listener = ListenerOptions::new().name(name).create_sync()?;
            Ok(listener)
        }

        fn serve(_listener: IpcListener) -> Never {
            // Reference impl returns by panic. Real implementers run an
            // accept loop and never return; the `Never` return type
            // means there is no `return` value they could construct.
            panic!("StubBackend::serve called");
        }
    }

    /// Conformance test #1 (#497 acceptance): the `bind` method has no
    /// state parameter. Verified by the trait signature itself — if
    /// this test compiles, the property holds. Equivalent to the
    /// `trybuild` UI test in #497's acceptance criteria, expressed via
    /// the type system rather than a separate harness.
    #[test]
    fn brokered_backend_bind_returns_listener_from_endpoint_only() {
        // The line `fn bind(endpoint: &Endpoint) -> Result<...>`
        // structurally denies a `state` parameter. If a future revision
        // of the trait added one, this test would fail to compile.
        fn _shape_check<B: BrokeredBackend>() -> fn(&Endpoint) -> Result<IpcListener, BindError> {
            B::bind
        }
        let _ = _shape_check::<StubBackend>();
    }

    /// Conformance test #3 (#497 acceptance): an implementation that
    /// returns an actual listener from `bind` produces a probe-able
    /// endpoint immediately (no `serve` call required).
    #[test]
    fn bind_alone_yields_a_listening_endpoint() {
        let listener = StubBackend::bind("bind-alone").expect("bind succeeds");
        // The listener's `accept` is the broker's hang-detection probe
        // primitive. We don't call `accept` here (would block); we just
        // verify the listener was constructed by the daemon-side code
        // path without any state allocation.
        drop(listener);
    }

    /// With nothing handed over, `bootstrap` binds for itself — the
    /// behaviour every daemon has today, asserted so the adoption path
    /// added for #500 cannot silently change it.
    ///
    /// Reads the ambient environment rather than setting it: env-mutating
    /// tests race under a parallel runner, and this crate has been bitten by
    /// that. The variable is unset in a normal test process, which is exactly
    /// the case being asserted.
    #[test]
    fn without_a_handover_bootstrap_still_binds_for_itself() {
        if std::env::var_os(crate::broker::broker_owned_bind::INHERITED_LISTENER_FD_ENV).is_some() {
            eprintln!("skipping: a listener descriptor is set in this environment");
            return;
        }
        // Reaching `serve` proves a listener was obtained. With no handover
        // the only way to get one is `B::bind`, and `StubBackend::serve`
        // panics — so the panic is the evidence.
        let result = std::panic::catch_unwind(|| bootstrap::<StubBackend>("no-handover"));
        let payload = result.expect_err("bootstrap should reach the serve panic");
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        assert!(
            message.contains("StubBackend::serve called"),
            "expected the self-bind path to reach serve, got: {message:?}"
        );
    }

    /// `bootstrap` orchestration calls `bind` first; only if `bind`
    /// succeeds does it hand off to `serve`. With this stub, `serve`
    /// panics — so a successful `bind` followed by the panic proves
    /// the orchestration ordering.
    #[test]
    fn bootstrap_calls_bind_then_serve() {
        let result = std::panic::catch_unwind(|| bootstrap::<StubBackend>("bootstrap-ordering"));
        let payload = result.expect_err("bootstrap should reach the serve panic");
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        assert!(
            message.contains("StubBackend::serve called"),
            "expected to reach serve, got panic payload: {message:?}"
        );
    }

    /// A `bind` failure short-circuits before `serve` runs. Uses a
    /// custom failing backend rather than tweaking the stub so the
    /// stub's "real bind succeeds" property stays intact for the other
    /// tests in this module.
    #[test]
    fn bootstrap_propagates_bind_failure_without_invoking_serve() {
        struct FailingBackend;
        impl BrokeredBackend for FailingBackend {
            type State = ();
            fn bind(_endpoint: &Endpoint) -> Result<IpcListener, BindError> {
                Err(BindError::Other("synthetic failure".into()))
            }
            fn serve(_listener: IpcListener) -> Never {
                panic!("serve must not run when bind fails");
            }
        }

        let result = std::panic::catch_unwind(|| bootstrap::<FailingBackend>("bootstrap-failure"));
        // The orchestrator returns Err — never panics — when bind fails.
        // catch_unwind preserves that as Ok(Err(...)).
        let inner = result.expect("bind error should be returned, not a panic");
        match inner {
            Err(BindError::Other(msg)) => assert_eq!(msg, "synthetic failure"),
            other => panic!("expected BindError::Other, got: {other:?}"),
        }
    }
}
