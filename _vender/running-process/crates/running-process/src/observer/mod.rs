//! Phase 1 of #221: the process-observation capability model and the
//! portable process-lifecycle baseline.
//!
//! This module defines the stable observation types — [`ObserverConfig`],
//! [`ObserverCapabilities`], [`ObserverEvent`], and the
//! [`ObserverSubscriber`] handle — plus the always-available lifecycle
//! backend that emits [`started`](ObserverEventKind::Started) and
//! [`exited`](ObserverEventKind::Exited) events for child processes spawned
//! by this crate.
//!
//! ## TraceScope dimension (#539)
//!
//! The capability matrix is negotiated for a [`TraceScope`]:
//!
//! - [`TraceScope::SystemWide`] — the historical default, names admin-gated
//!   system tracers (ETW kernel providers, eBPF, EndpointSecurity). All
//!   syscall categories report [`Unavailable`](CapabilitySupport::Unavailable)
//!   until the Phase 3 backends from #469 land.
//! - [`TraceScope::LaunchedProcessTree`] — the no-admin tier added by #539.
//!   Names per-OS primitives that operate purely on the spawn boundary this
//!   crate already owns (Windows Job Object IOCP, Linux subreaper+pidfd,
//!   macOS kqueue EVFILT_PROC). Currently every syscall category reports
//!   `Unavailable`; each #539 slice flips one cell to `Supported`/`Partial`
//!   with no shape change.
//!
//! Lifecycle is `Supported` in every scope because owning the spawn boundary
//! is sufficient for `started`/`exited` on all three platforms.
//!
//! `ObserverCapabilities::negotiate()` preserves the pre-#539 contract and
//! returns the `SystemWide` matrix; new callers should use
//! [`negotiate_for_scope`](ObserverCapabilities::negotiate_for_scope).
//!
//! ## Off by default
//!
//! Observation is entirely opt-in. A [`NativeProcess`](crate::NativeProcess)
//! emits no events unless an [`ObserverConfig`] is attached via
//! [`NativeProcess::with_observer`](crate::NativeProcess::with_observer) (or
//! the equivalent builder seam). With no observer configured the lifecycle
//! hooks are inert: no channel, no allocation, no events.
//!
//! The handle is a plain `std::sync::mpsc` receiver so the lifecycle
//! baseline stays free of the daemon runtime (tokio/IPC). Phase 2 layers the
//! daemon-owned subscriber model on top of these same event types.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod cmdline;
pub use cmdline::read_process_cmdline;

mod file_handles;
pub use file_handles::read_process_file_handles;

#[cfg(target_os = "linux")]
pub(crate) mod descendants_linux;

#[cfg(target_os = "macos")]
pub(crate) mod descendants_macos;

#[cfg(any(test, target_os = "linux", target_os = "macos"))]
mod pid_identity;

/// Scope at which observation is negotiated.
///
/// `running-process` exposes two distinct observation tiers because the
/// underlying OS primitives diverge sharply by privilege:
///
/// - [`LaunchedProcessTree`](Self::LaunchedProcessTree) — observe the process
///   tree that this crate spawned and any descendants reparented under it.
///   No admin / no entitlements / no kernel driver required. The crate owns
///   the spawn boundary on every platform (Job Object on Windows, subreaper
///   on Linux, kqueue child registration on macOS), so per-platform
///   no-admin primitives are sufficient. This is the scope #539 wires up.
/// - [`SystemWide`](Self::SystemWide) — observe every process on the host.
///   Requires ETW kernel providers on Windows, eBPF/CAP_BPF on Linux,
///   Endpoint Security entitlement on macOS. All of these need admin or
///   signed entitlements and a separate operational story (#469).
///
/// The two scopes can coexist; backends for each are detected and reported
/// independently. Marked `#[non_exhaustive]` per #431 so future scopes
/// (e.g. cgroup-scoped, container-scoped) can land without a major bump.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceScope {
    /// Observation limited to the process tree this crate spawned.
    /// Backends for this scope must operate without admin privileges.
    LaunchedProcessTree,
    /// Observation of every process on the host. Backends typically
    /// require admin / entitlements / kernel drivers.
    SystemWide,
}

impl TraceScope {
    /// All scopes in stable order.
    pub const ALL: [TraceScope; 2] = [TraceScope::LaunchedProcessTree, TraceScope::SystemWide];

    /// Stable lowercase name for serialization / matrix rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceScope::LaunchedProcessTree => "launched-process-tree",
            TraceScope::SystemWide => "system-wide",
        }
    }
}

/// Category of observable process activity.
///
/// Phase 1 only implements [`Lifecycle`](Self::Lifecycle). The remaining
/// categories exist so capability negotiation can report them as
/// `unavailable` with an honest reason until their Phase 3 platform backends
/// land.
///
/// Marked `#[non_exhaustive]` per #431: Phase 3 will refine these categories
/// (and possibly add sub-categories) without forcing every consumer to bump
/// to a new major version of the crate. Out-of-crate matchers must include a
/// wildcard arm.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCategory {
    /// Process start and exit for children spawned by this crate.
    Lifecycle,
    /// Filesystem activity (open/read/write/unlink). Requires a Phase 3
    /// platform backend.
    File,
    /// Network activity (connect/accept/send/recv). Requires a Phase 3
    /// platform backend.
    Network,
    /// Descendant process creation outside the crate's own spawn path.
    /// Requires a Phase 3 platform backend.
    Process,
}

impl EventCategory {
    /// All categories the capability matrix reports on, in a stable order.
    pub const ALL: [EventCategory; 4] = [
        EventCategory::Lifecycle,
        EventCategory::File,
        EventCategory::Network,
        EventCategory::Process,
    ];

    /// Return the stable lowercase category name.
    pub fn as_str(self) -> &'static str {
        match self {
            EventCategory::Lifecycle => "lifecycle",
            EventCategory::File => "file",
            EventCategory::Network => "network",
            EventCategory::Process => "process",
        }
    }
}

/// Negotiated support level for a single [`EventCategory`].
///
/// Marked `#[non_exhaustive]` per #431: later phases may introduce richer
/// support gradations (e.g. a `Degraded` variant distinct from `Partial`)
/// without breaking out-of-crate matchers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySupport {
    /// The category is fully observable on this platform.
    Supported,
    /// The category is observable but with documented gaps or caveats.
    Partial,
    /// The category cannot be observed by the active backend set.
    Unavailable,
}

impl CapabilitySupport {
    /// Return the stable lowercase support-level name.
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilitySupport::Supported => "supported",
            CapabilitySupport::Partial => "partial",
            CapabilitySupport::Unavailable => "unavailable",
        }
    }
}

/// Capability report for one [`EventCategory`]: the negotiated support
/// level, the backend that would serve it, and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryCapability {
    /// Which category this entry describes.
    pub category: EventCategory,
    /// Negotiated support level.
    pub support: CapabilitySupport,
    /// Name of the backend serving (or that would serve) this category.
    pub backend: &'static str,
    /// Human-readable explanation, especially for `Partial`/`Unavailable`.
    pub reason: &'static str,
}

/// The full capability matrix produced by [`ObserverCapabilities::negotiate`]
/// or [`ObserverCapabilities::negotiate_for_scope`].
///
/// Each [`EventCategory`] appears exactly once for the negotiated
/// [`TraceScope`]. Phase 1 reports [`Lifecycle`](EventCategory::Lifecycle) as
/// [`Supported`](CapabilitySupport::Supported) in every scope (the spawn/reap
/// path is scope-independent); the rest start out as
/// [`Unavailable`](CapabilitySupport::Unavailable) and flip to
/// `Supported`/`Partial` as per-OS backends land (#539 for
/// [`LaunchedProcessTree`](TraceScope::LaunchedProcessTree), #469 for
/// [`SystemWide`](TraceScope::SystemWide)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverCapabilities {
    scope: TraceScope,
    categories: Vec<CategoryCapability>,
}

/// Detect the backend that would serve [`EventCategory::File`] on this
/// platform for the requested [`TraceScope`].
///
/// Returns `(support, backend, reason)`. Today every branch returns
/// `Unavailable`. As individual backends land, flip the matching branch to
/// `Supported`/`Partial` with no shape change.
///
/// Scope split:
///
/// - [`TraceScope::SystemWide`] — names the admin-gated system tracer that
///   would have to land (ETW kernel provider, eBPF, EndpointSecurity).
///   Tracked by #469.
/// - [`TraceScope::LaunchedProcessTree`] — names the no-admin per-OS
///   primitive that observes only this crate's spawned tree
///   (NT handle snapshot, `/proc/<pid>/fd/*`, `proc_pidinfo`). Tracked by
///   #539. Lands incrementally per slice.
fn detect_file_backend(scope: TraceScope) -> (CapabilitySupport, &'static str, &'static str) {
    match scope {
        TraceScope::SystemWide => {
            #[cfg(target_os = "linux")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "seccomp-user-notify",
                    "Phase 3: Linux seccomp user-notify file backend not yet implemented",
                )
            }
            #[cfg(target_os = "windows")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "etw",
                    "Phase 3: Windows ETW file backend not yet implemented",
                )
            }
            #[cfg(target_os = "macos")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "kqueue",
                    "Phase 3: macOS kqueue/EndpointSecurity file backend not yet implemented (entitlement-gated)",
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                (
                    CapabilitySupport::Unavailable,
                    "none",
                    "Phase 3: no file backend planned for this OS",
                )
            }
        }
        TraceScope::LaunchedProcessTree => {
            #[cfg(target_os = "linux")]
            {
                (
                    CapabilitySupport::Partial,
                    "proc-fd-snapshot",
                    "Linux /proc/<pid>/fd/* snapshot via read_process_file_handles (#539 slice 6 follow-up; no streaming file events)",
                )
            }
            #[cfg(target_os = "windows")]
            {
                (
                    CapabilitySupport::Partial,
                    "nt-handle-snapshot",
                    "Windows NtQuerySystemInformation + DuplicateHandle + NtQueryObject snapshot via read_process_file_handles (#539 slice 4; no streaming file events)",
                )
            }
            #[cfg(target_os = "macos")]
            {
                (
                    CapabilitySupport::Partial,
                    "proc-pidinfo",
                    "macOS proc_pidinfo(PROC_PIDLISTFDS) snapshot via read_process_file_handles (#539 slice 8 follow-up; no streaming file events)",
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                (
                    CapabilitySupport::Unavailable,
                    "none",
                    "#539: no launched-process-tree file backend planned for this OS",
                )
            }
        }
    }
}

/// Detect the backend that would serve [`EventCategory::Network`] on this
/// platform for the requested [`TraceScope`]. Mirrors [`detect_file_backend`].
///
/// Network observation is deferred to a future issue for the
/// [`TraceScope::LaunchedProcessTree`] scope — there is no portable
/// no-admin primitive for per-child connect/accept events comparable to the
/// file/process primitives, so the backend is currently `none` everywhere
/// for that scope.
fn detect_network_backend(scope: TraceScope) -> (CapabilitySupport, &'static str, &'static str) {
    match scope {
        TraceScope::SystemWide => {
            #[cfg(target_os = "linux")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "ebpf",
                    "Phase 3: Linux eBPF network backend not yet implemented",
                )
            }
            #[cfg(target_os = "windows")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "etw",
                    "Phase 3: Windows ETW network backend not yet implemented",
                )
            }
            #[cfg(target_os = "macos")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "endpoint-security",
                    "Phase 3: macOS EndpointSecurity network backend not yet implemented (entitlement-gated)",
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                (
                    CapabilitySupport::Unavailable,
                    "none",
                    "Phase 3: no network backend planned for this OS",
                )
            }
        }
        TraceScope::LaunchedProcessTree => (
            CapabilitySupport::Unavailable,
            "none",
            "#539: no-admin per-child network backend deferred to a follow-up issue",
        ),
    }
}

/// Detect the backend that would serve [`EventCategory::Process`] (descendant
/// process creation outside the crate's own spawn path) on this platform
/// for the requested [`TraceScope`]. Mirrors [`detect_file_backend`].
fn detect_process_backend(scope: TraceScope) -> (CapabilitySupport, &'static str, &'static str) {
    match scope {
        TraceScope::SystemWide => {
            #[cfg(target_os = "linux")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "seccomp-user-notify",
                    "Phase 3: Linux seccomp user-notify process backend not yet implemented",
                )
            }
            #[cfg(target_os = "windows")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "etw",
                    "Phase 3: Windows ETW process backend not yet implemented",
                )
            }
            #[cfg(target_os = "macos")]
            {
                (
                    CapabilitySupport::Unavailable,
                    "endpoint-security",
                    "Phase 3: macOS EndpointSecurity process backend not yet implemented (entitlement-gated)",
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                (
                    CapabilitySupport::Unavailable,
                    "none",
                    "Phase 3: no process backend planned for this OS",
                )
            }
        }
        TraceScope::LaunchedProcessTree => {
            #[cfg(target_os = "linux")]
            {
                (
                    CapabilitySupport::Supported,
                    "subreaper-proc-poll",
                    "Linux PR_SET_CHILD_SUBREAPER + /proc descendant polling (#539 slice 5)",
                )
            }
            #[cfg(target_os = "windows")]
            {
                (
                    CapabilitySupport::Supported,
                    "job-object-iocp",
                    "Windows Job Object IOCP descendant lifecycle (#539 slice 2)",
                )
            }
            #[cfg(target_os = "macos")]
            {
                (
                    CapabilitySupport::Supported,
                    "sysctl-proc-poll",
                    "macOS sysctl(KERN_PROC_ALL) descendant polling (#539 slice 7)",
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                (
                    CapabilitySupport::Unavailable,
                    "none",
                    "#539: no launched-process-tree process backend planned for this OS",
                )
            }
        }
    }
}

impl ObserverCapabilities {
    /// Negotiate the capability matrix for the current platform under the
    /// historical default scope ([`TraceScope::SystemWide`]).
    ///
    /// Preserved for backwards compatibility with pre-#539 callers. New
    /// callers that know which tier they want should use
    /// [`negotiate_for_scope`](Self::negotiate_for_scope) — the
    /// `LaunchedProcessTree` scope advertises different per-OS backends
    /// (no-admin: NT handle snapshot, `/proc/<pid>/fd/*`, `proc_pidinfo`)
    /// than the `SystemWide` scope (admin-gated: ETW, eBPF, EndpointSecurity).
    pub fn negotiate() -> Self {
        Self::negotiate_for_scope(TraceScope::SystemWide)
    }

    /// Negotiate the capability matrix for the current platform at the
    /// requested [`TraceScope`].
    ///
    /// Lifecycle is `Supported` in every scope (the spawn/reap path is
    /// scope-independent and runs in-process with no admin requirement).
    /// File/Network/Process start out `Unavailable` and flip to
    /// `Supported`/`Partial` as per-OS backends land — the scope × OS
    /// dispatch lives in the crate-private `detect_*_backend` helpers.
    pub fn negotiate_for_scope(scope: TraceScope) -> Self {
        let categories = EventCategory::ALL
            .iter()
            .map(|&category| match category {
                EventCategory::Lifecycle => CategoryCapability {
                    category,
                    support: CapabilitySupport::Supported,
                    backend: "portable-lifecycle",
                    reason: "started/exited emitted from the crate spawn and reap path",
                },
                EventCategory::File => {
                    let (support, backend, reason) = detect_file_backend(scope);
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
                EventCategory::Network => {
                    let (support, backend, reason) = detect_network_backend(scope);
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
                EventCategory::Process => {
                    let (support, backend, reason) = detect_process_backend(scope);
                    CategoryCapability {
                        category,
                        support,
                        backend,
                        reason,
                    }
                }
            })
            .collect();
        Self { scope, categories }
    }

    /// The [`TraceScope`] this matrix was negotiated for.
    pub fn scope(&self) -> TraceScope {
        self.scope
    }

    /// Return the capability entries in stable [`EventCategory::ALL`] order.
    pub fn categories(&self) -> &[CategoryCapability] {
        &self.categories
    }

    /// Look up the capability entry for one category.
    pub fn category(&self, category: EventCategory) -> &CategoryCapability {
        self.categories
            .iter()
            .find(|entry| entry.category == category)
            .expect("ObserverCapabilities always contains every EventCategory")
    }

    /// Return the negotiated support level for one category.
    pub fn support(&self, category: EventCategory) -> CapabilitySupport {
        self.category(category).support
    }

    /// Return whether a category is fully [`Supported`](CapabilitySupport::Supported).
    pub fn is_supported(&self, category: EventCategory) -> bool {
        self.support(category) == CapabilitySupport::Supported
    }

    /// Return the capability matrix as four fixed-width rows suitable for
    /// downstream UX (e.g. a clud CLI flag — see Phase 4 of #221 / #431).
    ///
    /// Each row is `[category, support, backend, reason]`. Row order matches
    /// [`EventCategory::ALL`], so consumers can rely on a stable layout. The
    /// strings are owned so callers can paint colors / pad columns without
    /// borrowing from `self`.
    pub fn to_table_rows(&self) -> Vec<[String; 4]> {
        self.categories
            .iter()
            .map(|entry| {
                [
                    entry.category.as_str().to_string(),
                    entry.support.as_str().to_string(),
                    entry.backend.to_string(),
                    entry.reason.to_string(),
                ]
            })
            .collect()
    }

    /// Render the capability matrix as a single human-readable string.
    ///
    /// The output is deterministic per scope+category set so a UI can
    /// snapshot or diff it. The first line names the negotiated
    /// [`TraceScope`] so a diff between scopes is obvious. Layout:
    ///
    /// ```text
    /// observer capabilities (scope=system-wide):
    ///   lifecycle    supported    portable-lifecycle  started/exited emitted from the crate spawn and reap path
    ///   file         unavailable  etw                 Phase 3: Windows ETW file backend not yet implemented
    ///   network      unavailable  etw                 Phase 3: Windows ETW network backend not yet implemented
    ///   process      unavailable  etw                 Phase 3: Windows ETW process backend not yet implemented
    /// ```
    ///
    /// Phase 4 (#431) consumers like the clud CLI use this to show the
    /// actually negotiated matrix rather than claiming syscall coverage the
    /// active backends do not provide.
    pub fn render_summary(&self) -> String {
        // Compute column widths from the longest entry per column so the
        // output stays aligned as future categories / backends land.
        let rows = self.to_table_rows();
        let mut widths = [0usize; 3];
        for row in &rows {
            for (i, cell) in row[..3].iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let mut out = format!("observer capabilities (scope={}):\n", self.scope.as_str());
        for row in &rows {
            out.push_str(&format!(
                "  {cat:<cw$}  {sup:<sw$}  {bk:<bw$}  {reason}\n",
                cat = row[0],
                sup = row[1],
                bk = row[2],
                reason = row[3],
                cw = widths[0],
                sw = widths[1],
                bw = widths[2],
            ));
        }
        out
    }
}

/// What happened to an observed process.
///
/// Marked `#[non_exhaustive]` per #431: Phase 3 will add variants for File,
/// Network, and Process events. Out-of-crate matchers must include a
/// wildcard arm to remain forward-compatible across minor releases.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverEventKind {
    /// The child process was spawned. Carries no extra payload.
    Started,
    /// The child process exited. Carries the OS exit code (Unix signal
    /// exits are negative signal numbers, matching the rest of the crate).
    Exited {
        /// Exit code of the child.
        exit_code: i32,
    },
    /// A descendant of the spawned process (i.e. a child of a child) was
    /// created. Emitted on the [`EventCategory::Process`] category by
    /// per-OS LaunchedProcessTree backends (#539). The descendant PID is
    /// carried by [`ObserverEvent::pid`].
    ///
    /// Unlike [`Started`](Self::Started), this carries no exit code on the
    /// pair event because the no-admin descendant-lifecycle primitives
    /// (Windows Job Object IOCP, Linux pidfd reap, macOS `EVFILT_PROC`)
    /// surface PID-only notifications.
    DescendantStarted,
    /// A descendant process exited. Emitted on the
    /// [`EventCategory::Process`] category by per-OS LaunchedProcessTree
    /// backends (#539). The descendant PID is carried by
    /// [`ObserverEvent::pid`]; the exit code is not surfaced — see
    /// [`DescendantStarted`](Self::DescendantStarted) for rationale.
    DescendantExited,
    /// A file was opened by the observed process. Emitted on the
    /// [`EventCategory::File`] category by the **hook tier** of the
    /// observer (the sidecar interposer in `running-process-probe`,
    /// tracked by #551). The pid in [`ObserverEvent::pid`] is the
    /// process that performed the call. `flags` is the platform-native
    /// open flags (POSIX `O_*` on Unix; Windows `dwDesiredAccess` |
    /// `(dwShareMode << 16)` encoded best-effort).
    FileOpen {
        /// Filesystem path the consumer opened. On Linux/macOS this is
        /// the POSIX path passed to `open(2)` / `openat(2)`; on Windows
        /// it's the resolved Win32 path (DOS-form when available, NT
        /// path otherwise — matches the slice-4 #550 convention).
        path: std::path::PathBuf,
        /// Platform-native open flags. Best-effort encoded.
        flags: u32,
    },
    /// A file was written to by the observed process. `byte_count` is
    /// the byte count returned by the syscall on success (may be
    /// shorter than the request on short writes). Emitted on the
    /// [`EventCategory::File`] category by the hook tier (#551).
    FileWrite {
        /// Resolved path of the file the write targeted.
        path: std::path::PathBuf,
        /// Number of bytes the syscall reported it actually wrote.
        byte_count: u64,
    },
    /// A file descriptor / handle was closed. Emitted on the
    /// [`EventCategory::File`] category by the hook tier (#551). The
    /// path is resolved at hook-fire time from the fd / handle, so it
    /// matches the path the corresponding [`FileOpen`](Self::FileOpen)
    /// event reported.
    FileClose {
        /// Resolved path of the file that was closed.
        path: std::path::PathBuf,
    },
    /// A file was unlinked / deleted. Emitted on the
    /// [`EventCategory::File`] category by the hook tier (#551).
    FileUnlink {
        /// Path of the file that was unlinked.
        path: std::path::PathBuf,
    },
    /// A file was renamed. Emitted on the [`EventCategory::File`]
    /// category by the hook tier (#551).
    FileRename {
        /// Path the file was renamed from.
        from: std::path::PathBuf,
        /// Path the file was renamed to.
        to: std::path::PathBuf,
    },
}

impl ObserverEventKind {
    /// Return the stable lowercase event-kind name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObserverEventKind::Started => "started",
            ObserverEventKind::Exited { .. } => "exited",
            ObserverEventKind::DescendantStarted => "descendant-started",
            ObserverEventKind::DescendantExited => "descendant-exited",
            ObserverEventKind::FileOpen { .. } => "file-open",
            ObserverEventKind::FileWrite { .. } => "file-write",
            ObserverEventKind::FileClose { .. } => "file-close",
            ObserverEventKind::FileUnlink { .. } => "file-unlink",
            ObserverEventKind::FileRename { .. } => "file-rename",
        }
    }
}

/// A single observation emitted by the lifecycle baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverEvent {
    /// Which category produced the event. Always
    /// [`EventCategory::Lifecycle`] in Phase 1.
    pub category: EventCategory,
    /// What happened.
    pub kind: ObserverEventKind,
    /// OS process id of the observed child.
    pub pid: u32,
    /// Milliseconds since the Unix epoch when the event was recorded.
    pub timestamp_ms: u128,
}

impl ObserverEvent {
    /// Construct an event, stamping it with the current wall-clock time.
    fn now(category: EventCategory, kind: ObserverEventKind, pid: u32) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            category,
            kind,
            pid,
            timestamp_ms,
        }
    }

    /// Construct an event stamped with the current wall-clock time.
    ///
    /// Crate-public sibling of the private `now` constructor for the daemon's
    /// per-session observer registry (#221 Phase 2 / #429), which emits
    /// lifecycle events directly without going through the crate-private
    /// `ObserverEmitter`.
    pub fn new_now(category: EventCategory, kind: ObserverEventKind, pid: u32) -> Self {
        Self::now(category, kind, pid)
    }
}

/// Opt-in configuration that turns process observation on for a single
/// [`NativeProcess`](crate::NativeProcess).
///
/// Constructing a config does not by itself observe anything; it is attached
/// to a process via
/// [`NativeProcess::with_observer`](crate::NativeProcess::with_observer).
/// With no config attached, the process emits no events (off by default).
#[derive(Debug, Clone)]
pub struct ObserverConfig {
    categories: Vec<EventCategory>,
}

impl ObserverConfig {
    /// Create a config that observes only the Phase 1 lifecycle baseline.
    ///
    /// This is the recommended Phase 1 constructor: it requests exactly the
    /// category that is actually `Supported`.
    pub fn lifecycle() -> Self {
        Self {
            categories: vec![EventCategory::Lifecycle],
        }
    }

    /// Create a config requesting an explicit set of categories.
    ///
    /// Categories that are not `Supported` on this platform simply never
    /// produce events in Phase 1; callers should consult
    /// [`ObserverCapabilities::negotiate`] to learn which ones are honored.
    pub fn with_categories(categories: impl IntoIterator<Item = EventCategory>) -> Self {
        Self {
            categories: categories.into_iter().collect(),
        }
    }

    /// Return whether this config requested observation of `category`.
    pub fn observes(&self, category: EventCategory) -> bool {
        self.categories.contains(&category)
    }

    /// The categories this config requested, in insertion order.
    pub fn categories(&self) -> &[EventCategory] {
        &self.categories
    }
}

/// Receiver handle for observation events.
///
/// Returned by
/// [`NativeProcess::with_observer`](crate::NativeProcess::with_observer).
/// Dropping the subscriber detaches it; the emitter tolerates a closed
/// channel and never blocks on a slow or absent consumer.
pub struct ObserverSubscriber {
    rx: Receiver<ObserverEvent>,
    descendant_stop: Arc<DescendantPumpStop>,
}

impl ObserverSubscriber {
    /// Wrap an existing channel receiver. Used by the daemon client helpers
    /// in `client::observer` to hand the caller a subscriber whose channel
    /// is later fed by an IPC streaming pump.
    pub(crate) fn from_receiver(rx: Receiver<ObserverEvent>) -> Self {
        Self {
            rx,
            descendant_stop: Arc::new(DescendantPumpStop::new()),
        }
    }

    /// Receive the next event, blocking until one arrives or the emitter is
    /// dropped. Returns `None` once no more events can arrive.
    pub fn recv(&self) -> Option<ObserverEvent> {
        self.rx.recv().ok()
    }

    /// Receive the next event, waiting for at most `timeout`.
    ///
    /// Returns [`std::sync::mpsc::RecvTimeoutError::Timeout`] when the bound
    /// expires and [`std::sync::mpsc::RecvTimeoutError::Disconnected`] once no
    /// more events can arrive.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ObserverEvent, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Try to receive an event without blocking.
    pub fn try_recv(&self) -> Option<ObserverEvent> {
        self.rx.try_recv().ok()
    }

    /// Drain all currently-queued events without blocking.
    pub fn drain(&self) -> Vec<ObserverEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Borrow the underlying receiver for advanced use (e.g. `iter`/`select`).
    pub fn receiver(&self) -> &Receiver<ObserverEvent> {
        &self.rx
    }

    /// Stop any Linux/macOS descendant pump associated with this subscriber.
    ///
    /// This is idempotent and wakes a sleeping pump immediately. Lifecycle
    /// events already queued on the subscriber remain available.
    pub fn stop(&self) {
        self.descendant_stop.stop();
    }
}

impl Drop for ObserverSubscriber {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct DescendantPumpStop {
    stopped: AtomicBool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mutex: Mutex<()>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    wake: Condvar,
}

impl DescendantPumpStop {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            mutex: Mutex::new(()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            wake: Condvar::new(),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) fn stop(&self) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let _guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
            if !self.stopped.swap(true, Ordering::AcqRel) {
                self.wake.notify_all();
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.stopped.store(true, Ordering::Release);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_stopped() {
            return true;
        }
        let guard = self.mutex.lock().unwrap_or_else(|error| error.into_inner());
        if self.is_stopped() {
            return true;
        }
        let (_guard, _wait_result) = self
            .wake
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|error| error.into_inner());
        self.is_stopped()
    }
}

/// Internal emitter held by a [`NativeProcess`](crate::NativeProcess) when an
/// [`ObserverConfig`] is attached.
///
/// `None` on a process means observation is off, so the lifecycle hooks are
/// inert. This keeps the off-by-default path allocation-free.
pub(crate) struct ObserverEmitter {
    config: ObserverConfig,
    tx: Sender<ObserverEvent>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    descendant_stop: Arc<DescendantPumpStop>,
}

impl ObserverEmitter {
    /// Build an emitter from a config and hand back the paired subscriber.
    pub(crate) fn new(config: ObserverConfig) -> (Self, ObserverSubscriber) {
        let (tx, rx) = std::sync::mpsc::channel();
        let descendant_stop = Arc::new(DescendantPumpStop::new());
        (
            Self {
                config,
                tx,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                descendant_stop: Arc::clone(&descendant_stop),
            },
            ObserverSubscriber {
                rx,
                descendant_stop,
            },
        )
    }

    /// Emit a `started` event for `pid` if the config observes lifecycle.
    pub(crate) fn emit_started(&self, pid: u32) {
        if !self.config.observes(EventCategory::Lifecycle) {
            return;
        }
        // Ignore send errors: a dropped subscriber must never break the
        // process spawn/reap path.
        let _ = self.tx.send(ObserverEvent::now(
            EventCategory::Lifecycle,
            ObserverEventKind::Started,
            pid,
        ));
    }

    /// Emit an `exited` event for `pid` if the config observes lifecycle.
    pub(crate) fn emit_exited(&self, pid: u32, exit_code: i32) {
        if !self.config.observes(EventCategory::Lifecycle) {
            return;
        }
        let _ = self.tx.send(ObserverEvent::now(
            EventCategory::Lifecycle,
            ObserverEventKind::Exited { exit_code },
            pid,
        ));
    }

    /// Return a cloned sender for descendant lifecycle events if the config
    /// observes [`EventCategory::Process`]; otherwise `None`.
    ///
    /// Per-OS LaunchedProcessTree backends (#539) take this `Sender` and run
    /// a background pump (Windows Job Object IOCP, Linux pidfd reap, macOS
    /// `EVFILT_PROC`) that fires
    /// [`DescendantStarted`](ObserverEventKind::DescendantStarted) /
    /// [`DescendantExited`](ObserverEventKind::DescendantExited) on this
    /// channel. Returning `None` when Process isn't requested keeps the
    /// off-by-default path allocation-free.
    //
    // `dead_code`-allowed because only the Windows backend (slice 2)
    // currently consumes this; the Linux subreaper-pidfd backend (slice 5)
    // and macOS kqueue-evfilt-proc backend (slice 7) will plug in next.
    #[allow(dead_code)]
    pub(crate) fn descendant_sink(&self) -> Option<Sender<ObserverEvent>> {
        if self.config.observes(EventCategory::Process) {
            Some(self.tx.clone())
        } else {
            None
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn descendant_pump(
        &self,
    ) -> Option<(Sender<ObserverEvent>, Arc<DescendantPumpStop>)> {
        self.descendant_sink()
            .map(|sink| (sink, Arc::clone(&self.descendant_stop)))
    }
}

#[cfg(test)]
mod tests;
