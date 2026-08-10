//! Phase 1 tests for #221. Deterministic, fast, and OS-agnostic: they
//! drive a short-lived child via the platform shell so they run unchanged
//! in CI on Windows, macOS, and Linux.

use super::*;
use crate::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};
use std::time::{Duration, Instant};

/// A `ProcessConfig` that runs a child which exits immediately on every
/// platform (Unix: `sh -lc "exit 0"`; Windows: `cmd /C "exit 0"`).
fn quick_exit_config() -> ProcessConfig {
    ProcessConfig {
        command: CommandSpec::Shell("exit 0".to_string()),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    }
}

// ── Capability negotiation ──

#[test]
fn negotiate_reports_lifecycle_supported() {
    let caps = ObserverCapabilities::negotiate();
    assert!(caps.is_supported(EventCategory::Lifecycle));
    assert_eq!(
        caps.support(EventCategory::Lifecycle),
        CapabilitySupport::Supported
    );
    let entry = caps.category(EventCategory::Lifecycle);
    assert_eq!(entry.backend, "portable-lifecycle");
    assert!(!entry.reason.is_empty());
}

#[test]
fn negotiate_reports_syscall_categories_unavailable_with_reason() {
    let caps = ObserverCapabilities::negotiate();
    for category in [
        EventCategory::File,
        EventCategory::Network,
        EventCategory::Process,
    ] {
        let entry = caps.category(category);
        assert_eq!(
            entry.support,
            CapabilitySupport::Unavailable,
            "{} should be unavailable in Phase 1",
            category.as_str()
        );
        // Honest reason, mentioning the deferred Phase 3 backend.
        assert!(
            entry.reason.contains("Phase 3"),
            "{} reason must explain the deferral: {:?}",
            category.as_str(),
            entry.reason
        );
        assert!(!caps.is_supported(category));
    }
}

#[test]
fn syscall_categories_advertise_per_os_backend_name() {
    // #430: replace the catch-all `backend: "none"` / "(seccomp/eBPF/ETW)"
    // reason with per-OS detection helpers. The matrix still reports
    // Unavailable until Phase 3 backends actually land, but the backend
    // name now matches what's planned for the current target OS. This
    // makes Phase 4 downstream UX (the clud capability matrix) honest
    // about WHAT will land WHERE rather than implying coverage by
    // listing all three backends in the reason string.
    let caps = ObserverCapabilities::negotiate();
    let file = caps.category(EventCategory::File);
    let network = caps.category(EventCategory::Network);
    let process = caps.category(EventCategory::Process);

    #[cfg(target_os = "linux")]
    {
        assert_eq!(file.backend, "seccomp-user-notify");
        assert_eq!(network.backend, "ebpf");
        assert_eq!(process.backend, "seccomp-user-notify");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(file.backend, "etw");
        assert_eq!(network.backend, "etw");
        assert_eq!(process.backend, "etw");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(file.backend, "kqueue");
        assert_eq!(network.backend, "endpoint-security");
        assert_eq!(process.backend, "endpoint-security");
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        assert_eq!(file.backend, "none");
        assert_eq!(network.backend, "none");
        assert_eq!(process.backend, "none");
    }

    // The reason field no longer hardcodes the multi-backend "(seccomp/eBPF/ETW)"
    // literal — it points at the one backend planned for THIS OS.
    for entry in [file, network, process] {
        assert!(
            !entry.reason.contains("seccomp/eBPF/ETW"),
            "stale multi-backend reason: {:?}",
            entry.reason
        );
        assert!(
            entry.reason.contains("Phase 3"),
            "reason must keep the Phase 3 anchor: {:?}",
            entry.reason
        );
    }
}

#[test]
fn negotiate_covers_every_category_exactly_once() {
    let caps = ObserverCapabilities::negotiate();
    assert_eq!(caps.categories().len(), EventCategory::ALL.len());
    for category in EventCategory::ALL {
        // Does not panic — every category is present.
        let _ = caps.category(category);
    }
}

// ── Lifecycle baseline: started + exited ──

#[test]
fn observed_child_emits_started_then_exited() {
    let (process, subscriber) =
        NativeProcess::with_observer(quick_exit_config(), ObserverConfig::lifecycle());
    process.start().expect("spawn quick-exit child");
    let pid = process.pid().expect("child has a pid");
    let code = process
        .wait(Some(Duration::from_secs(30)))
        .expect("child exits");
    // Make sure the reaping path has flushed the exit event.
    process.close().ok();

    let events = subscriber.drain();
    assert_eq!(
        events.len(),
        2,
        "expected exactly started + exited, got {:?}",
        events
    );

    let started = &events[0];
    assert_eq!(started.category, EventCategory::Lifecycle);
    assert_eq!(started.kind, ObserverEventKind::Started);
    assert_eq!(started.kind.as_str(), "started");
    assert_eq!(started.pid, pid);

    let exited = &events[1];
    assert_eq!(exited.category, EventCategory::Lifecycle);
    assert_eq!(exited.kind, ObserverEventKind::Exited { exit_code: code });
    assert_eq!(exited.kind.as_str(), "exited");
    assert_eq!(exited.pid, pid);
    assert!(exited.timestamp_ms >= started.timestamp_ms);
}

#[test]
fn exited_event_is_emitted_exactly_once_across_paths() {
    // Drive every exit-observing path (wait + poll + close) and confirm the
    // guard collapses them to a single `exited`.
    let (process, subscriber) =
        NativeProcess::with_observer(quick_exit_config(), ObserverConfig::lifecycle());
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    let _ = process.poll();
    process.close().ok();

    let exited_count = subscriber
        .drain()
        .into_iter()
        .filter(|e| matches!(e.kind, ObserverEventKind::Exited { .. }))
        .count();
    assert_eq!(exited_count, 1, "exited must fire exactly once");
}

// ── Off by default ──

#[test]
fn no_events_when_observation_not_configured() {
    // `NativeProcess::new` attaches no observer. Run a child to completion
    // and prove the lifecycle hooks stayed inert (no channel, no events).
    let process = NativeProcess::new(quick_exit_config());
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    process.close().ok();
    // There is no subscriber to receive from — the proof is structural:
    // `new` returns only the process, never a subscriber handle. This test
    // exercises the off-by-default code path to ensure it does not panic.
}

#[test]
fn config_observes_only_requested_categories() {
    let lifecycle = ObserverConfig::lifecycle();
    assert!(lifecycle.observes(EventCategory::Lifecycle));
    assert!(!lifecycle.observes(EventCategory::File));

    let none = ObserverConfig::with_categories([]);
    assert!(!none.observes(EventCategory::Lifecycle));
}

#[test]
fn unobserved_category_produces_no_events() {
    // An observer that does NOT request lifecycle must stay silent even
    // though the process spawns and exits.
    let (process, subscriber) = NativeProcess::with_observer(
        quick_exit_config(),
        ObserverConfig::with_categories([EventCategory::File]),
    );
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30)));
    process.close().ok();
    assert!(
        subscriber.drain().is_empty(),
        "non-lifecycle observer must emit nothing in Phase 1"
    );
}

// ── Phase 4 (#431): render_summary / to_table_rows for downstream UX ──

#[test]
fn to_table_rows_one_row_per_category_in_stable_order() {
    let caps = ObserverCapabilities::negotiate();
    let rows = caps.to_table_rows();
    assert_eq!(rows.len(), EventCategory::ALL.len());
    let expected_cats: Vec<&str> = EventCategory::ALL.iter().map(|c| c.as_str()).collect();
    let actual_cats: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(actual_cats, expected_cats);
}

#[test]
fn to_table_rows_carries_support_backend_and_reason() {
    let caps = ObserverCapabilities::negotiate();
    let rows = caps.to_table_rows();
    let lifecycle = rows
        .iter()
        .find(|r| r[0] == "lifecycle")
        .expect("lifecycle row");
    assert_eq!(lifecycle[1], "supported");
    assert_eq!(lifecycle[2], "portable-lifecycle");
    assert!(!lifecycle[3].is_empty());
}

#[test]
fn render_summary_lists_every_category_and_aligns_columns() {
    let summary = ObserverCapabilities::negotiate().render_summary();
    assert!(
        summary.starts_with("observer capabilities (scope=system-wide):\n"),
        "summary must lead with the scope header: {summary:?}"
    );
    // Every category name shows up in the rendered output.
    for category in EventCategory::ALL {
        assert!(
            summary.contains(category.as_str()),
            "{} should appear in summary:\n{}",
            category.as_str(),
            summary
        );
    }
    // Every line after the header has the leading "  " indent (alignment
    // contract; a snapshot consumer can rely on this).
    let body: Vec<&str> = summary.lines().skip(1).filter(|l| !l.is_empty()).collect();
    assert_eq!(body.len(), EventCategory::ALL.len());
    for line in &body {
        assert!(
            line.starts_with("  "),
            "row missing two-space indent: {line:?}"
        );
    }
}

// ── Stable string forms ──

#[test]
fn category_and_support_string_forms_are_stable() {
    assert_eq!(EventCategory::Lifecycle.as_str(), "lifecycle");
    assert_eq!(EventCategory::File.as_str(), "file");
    assert_eq!(EventCategory::Network.as_str(), "network");
    assert_eq!(EventCategory::Process.as_str(), "process");
    assert_eq!(CapabilitySupport::Supported.as_str(), "supported");
    assert_eq!(CapabilitySupport::Partial.as_str(), "partial");
    assert_eq!(CapabilitySupport::Unavailable.as_str(), "unavailable");
}

// ── #539: TraceScope dimension ──

#[test]
fn trace_scope_string_forms_are_stable() {
    assert_eq!(
        TraceScope::LaunchedProcessTree.as_str(),
        "launched-process-tree"
    );
    assert_eq!(TraceScope::SystemWide.as_str(), "system-wide");
    assert_eq!(TraceScope::ALL.len(), 2);
}

#[test]
fn negotiate_defaults_to_system_wide_scope() {
    // Pre-#539 callers used `negotiate()` and got the SystemWide matrix.
    // Preserve that behavior so existing UX (e.g. clud capability table)
    // does not silently flip to the new scope.
    let caps = ObserverCapabilities::negotiate();
    assert_eq!(caps.scope(), TraceScope::SystemWide);
}

#[test]
fn negotiate_for_each_scope_yields_lifecycle_supported() {
    // The portable started/exited baseline is scope-independent — owning
    // the spawn boundary is sufficient on every platform.
    for scope in TraceScope::ALL {
        let caps = ObserverCapabilities::negotiate_for_scope(scope);
        assert_eq!(caps.scope(), scope);
        assert_eq!(
            caps.support(EventCategory::Lifecycle),
            CapabilitySupport::Supported,
            "lifecycle must be supported under scope={}",
            scope.as_str()
        );
        let entry = caps.category(EventCategory::Lifecycle);
        assert_eq!(entry.backend, "portable-lifecycle");
    }
}

#[test]
fn launched_process_tree_scope_advertises_no_admin_backends() {
    // The whole point of the LaunchedProcessTree scope is that its backend
    // names are no-admin per-OS primitives, distinct from the admin-gated
    // SystemWide backends. PRs 2–8 of #539 flip these from Unavailable to
    // Supported one cell at a time; the names are the stable contract.
    let caps = ObserverCapabilities::negotiate_for_scope(TraceScope::LaunchedProcessTree);
    let file = caps.category(EventCategory::File);
    let process = caps.category(EventCategory::Process);
    let network = caps.category(EventCategory::Network);

    #[cfg(target_os = "linux")]
    {
        assert_eq!(file.backend, "proc-fd-snapshot");
        assert_eq!(process.backend, "subreaper-proc-poll");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(file.backend, "nt-handle-snapshot");
        assert_eq!(process.backend, "job-object-iocp");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(file.backend, "proc-pidinfo");
        assert_eq!(process.backend, "sysctl-proc-poll");
    }

    // Network is uniformly deferred for this scope — no admin-free
    // per-child primitive exists across all three platforms.
    assert_eq!(network.backend, "none");

    // Every File/Network/Process reason in this scope must point at #539
    // so a reader knows which ledger to consult.
    for entry in [file, process, network] {
        assert!(
            entry.reason.contains("#539"),
            "LaunchedProcessTree reason must reference #539: {:?}",
            entry.reason
        );
    }
}

// ── #539 slice 2: Windows Job Object IOCP descendant lifecycle ──

#[test]
fn descendant_event_kind_string_forms_are_stable() {
    assert_eq!(
        ObserverEventKind::DescendantStarted.as_str(),
        "descendant-started"
    );
    assert_eq!(
        ObserverEventKind::DescendantExited.as_str(),
        "descendant-exited"
    );
}

// ── #551 slice 3: file-hook tier event variants ──

#[test]
fn file_hook_event_kind_string_forms_are_stable() {
    // These are the hook-tier event names the running-process-probe
    // sidecar (#551) will emit. Locking the lowercase forms now keeps
    // serialization stable across the interposer payloads landing in
    // slices 4–6 of #551.
    let pb = std::path::PathBuf::from("/tmp/x");
    assert_eq!(
        ObserverEventKind::FileOpen {
            path: pb.clone(),
            flags: 0
        }
        .as_str(),
        "file-open"
    );
    assert_eq!(
        ObserverEventKind::FileWrite {
            path: pb.clone(),
            byte_count: 0
        }
        .as_str(),
        "file-write"
    );
    assert_eq!(
        ObserverEventKind::FileClose { path: pb.clone() }.as_str(),
        "file-close"
    );
    assert_eq!(
        ObserverEventKind::FileUnlink { path: pb.clone() }.as_str(),
        "file-unlink"
    );
    assert_eq!(
        ObserverEventKind::FileRename {
            from: pb.clone(),
            to: pb.clone(),
        }
        .as_str(),
        "file-rename"
    );
}

#[test]
fn file_hook_event_kinds_carry_path_and_count_payloads() {
    // Sanity check that the variants round-trip their payloads
    // through pattern matching — locks the schema before slices 4–6
    // of #551 start writing emit sites.
    let path = std::path::PathBuf::from("/tmp/some-file.txt");
    let ev = ObserverEventKind::FileOpen {
        path: path.clone(),
        flags: 0o644,
    };
    if let ObserverEventKind::FileOpen { path: p, flags } = ev {
        assert_eq!(p, path);
        assert_eq!(flags, 0o644);
    } else {
        panic!("variant did not match")
    }

    let ev = ObserverEventKind::FileWrite {
        path: path.clone(),
        byte_count: 1024,
    };
    if let ObserverEventKind::FileWrite { byte_count, .. } = ev {
        assert_eq!(byte_count, 1024);
    } else {
        panic!("variant did not match")
    }

    let from = std::path::PathBuf::from("/tmp/a");
    let to = std::path::PathBuf::from("/tmp/b");
    let ev = ObserverEventKind::FileRename {
        from: from.clone(),
        to: to.clone(),
    };
    if let ObserverEventKind::FileRename { from: f, to: t } = ev {
        assert_eq!(f, from);
        assert_eq!(t, to);
    } else {
        panic!("variant did not match")
    }
}

#[test]
fn observer_event_can_carry_file_hook_variants() {
    // ObserverEvent's struct must accept the new ObserverEventKind
    // variants without further changes — pid + timestamp are
    // category-agnostic. File events use EventCategory::File.
    let ev = ObserverEvent::new_now(
        EventCategory::File,
        ObserverEventKind::FileOpen {
            path: "/tmp/lock-file".into(),
            flags: 0,
        },
        std::process::id(),
    );
    assert_eq!(ev.category, EventCategory::File);
    assert_eq!(ev.kind.as_str(), "file-open");
}

#[cfg(target_os = "windows")]
#[test]
fn windows_process_backend_supported_on_launched_process_tree_scope() {
    // Slice 2 flips Windows Process from Unavailable → Supported under
    // the LaunchedProcessTree scope. Lock the contract so a future
    // refactor cannot silently downgrade it.
    let caps = ObserverCapabilities::negotiate_for_scope(TraceScope::LaunchedProcessTree);
    let process = caps.category(EventCategory::Process);
    assert_eq!(
        process.support,
        CapabilitySupport::Supported,
        "Windows LaunchedProcessTree Process backend should be Supported after slice 2"
    );
    assert_eq!(process.backend, "job-object-iocp");
    assert!(
        process.reason.contains("#539 slice 2"),
        "reason should anchor to the slice that flipped it: {:?}",
        process.reason
    );
    // Sanity check: the SystemWide matrix is untouched — Windows ETW
    // process backend stays Unavailable per #469.
    let sw = ObserverCapabilities::negotiate_for_scope(TraceScope::SystemWide);
    let sw_process = sw.category(EventCategory::Process);
    assert_eq!(sw_process.support, CapabilitySupport::Unavailable);
    assert_eq!(sw_process.backend, "etw");
}

#[cfg(target_os = "windows")]
#[test]
fn windows_iocp_pump_emits_descendant_lifecycle_for_subprocess_chain() {
    // Direct child: cmd /C — that cmd then runs three nested `cmd /C exit 0`
    // subprocesses sequentially via &&. The outer cmd is direct_pid
    // (suppressed in the pump), each of the three inner cmds is a
    // descendant in the Job Object, so the IOCP should fire at least
    // three DescendantStarted + DescendantExited pairs.
    let cfg = ProcessConfig {
        command: CommandSpec::Shell("cmd /C exit 0 && cmd /C exit 0 && cmd /C exit 0".to_string()),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    };
    let (process, subscriber) = NativeProcess::with_observer(
        cfg,
        ObserverConfig::with_categories([EventCategory::Process]),
    );
    process.start().expect("spawn shell chain");
    let _ = process
        .wait(Some(Duration::from_secs(30)))
        .expect("shell chain exits");
    process.close().ok();

    // The IOCP pump exits on ACTIVE_PROCESS_ZERO, which fires once every
    // process in the Job has exited. Give it a brief grace period to
    // flush queued events to the subscriber's mpsc channel before we
    // drain.
    std::thread::sleep(Duration::from_millis(750));

    let events = subscriber.drain();
    let started: Vec<&ObserverEvent> = events
        .iter()
        .filter(|e| {
            e.category == EventCategory::Process
                && matches!(e.kind, ObserverEventKind::DescendantStarted)
        })
        .collect();
    let exited: Vec<&ObserverEvent> = events
        .iter()
        .filter(|e| {
            e.category == EventCategory::Process
                && matches!(e.kind, ObserverEventKind::DescendantExited)
        })
        .collect();

    assert!(
        started.len() >= 3,
        "expected ≥3 DescendantStarted events for the cmd chain, got {} (all events: {:?})",
        started.len(),
        events
    );
    assert!(
        exited.len() >= 3,
        "expected ≥3 DescendantExited events for the cmd chain, got {} (all events: {:?})",
        exited.len(),
        events
    );

    // No Lifecycle events should appear: the config asked for Process
    // only, so the direct child's started/exited stays suppressed.
    for ev in &events {
        assert_eq!(
            ev.category,
            EventCategory::Process,
            "Lifecycle category leaked into a Process-only subscriber: {ev:?}"
        );
    }
}

#[cfg(target_os = "windows")]
#[test]
fn windows_iocp_pump_inert_when_only_lifecycle_requested() {
    // Off-by-default contract: if the consumer only asked for Lifecycle,
    // the descendant_sink is None and no Process events are emitted even
    // when the child spawns descendants. The pump thread should not
    // start.
    let cfg = ProcessConfig {
        command: CommandSpec::Shell("cmd /C exit 0 && cmd /C exit 0".to_string()),
        cwd: None,
        env: None,
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
    };
    let (process, subscriber) = NativeProcess::with_observer(cfg, ObserverConfig::lifecycle());
    process.start().expect("spawn");
    let _ = process.wait(Some(Duration::from_secs(30))).expect("wait");
    process.close().ok();
    std::thread::sleep(Duration::from_millis(200));

    let events = subscriber.drain();
    for ev in &events {
        assert_ne!(
            ev.category,
            EventCategory::Process,
            "Process event leaked into a Lifecycle-only subscriber: {ev:?}"
        );
    }
    // Lifecycle still fires exactly Started + Exited for the direct child.
    let lifecycle: Vec<&ObserverEvent> = events
        .iter()
        .filter(|e| e.category == EventCategory::Lifecycle)
        .collect();
    assert_eq!(
        lifecycle.len(),
        2,
        "expected exactly started + exited for the direct child, got {lifecycle:?}"
    );
}

#[test]
fn descendant_sink_is_some_only_when_process_category_observed() {
    // The IOCP pump only spins up when the consumer actually requested
    // descendant observation. With Lifecycle-only config (the most common
    // path) the sink stays None and the off-by-default allocation
    // contract is preserved.
    let (emitter, _sub) = ObserverEmitter::new(ObserverConfig::lifecycle());
    assert!(
        emitter.descendant_sink().is_none(),
        "lifecycle-only observer must not allocate a descendant sink"
    );

    let (emitter, _sub) =
        ObserverEmitter::new(ObserverConfig::with_categories([EventCategory::Process]));
    assert!(
        emitter.descendant_sink().is_some(),
        "Process-observing config must hand back a sink for the IOCP pump"
    );

    let (emitter, _sub) = ObserverEmitter::new(ObserverConfig::with_categories([
        EventCategory::Lifecycle,
        EventCategory::Process,
    ]));
    assert!(
        emitter.descendant_sink().is_some(),
        "config including Process must hand back a sink even alongside Lifecycle"
    );
}

#[test]
fn subscriber_recv_timeout_is_bounded_and_normal_events_still_arrive() {
    let (emitter, subscriber) = ObserverEmitter::new(ObserverConfig::lifecycle());
    let started = Instant::now();
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(20)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(10),
        "subscriber timeout returned without waiting"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "subscriber timeout exceeded its bound"
    );

    emitter.emit_started(42);
    let event = subscriber
        .recv_timeout(Duration::from_secs(1))
        .expect("normal lifecycle event");
    assert_eq!(event.pid, 42);
    assert!(matches!(event.kind, ObserverEventKind::Started));

    drop(emitter);
    assert_eq!(
        subscriber.recv_timeout(Duration::from_secs(1)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn subscriber_stop_is_idempotent_and_signals_descendant_pump() {
    let (emitter, subscriber) =
        ObserverEmitter::new(ObserverConfig::with_categories([EventCategory::Process]));
    let (_, stop) = emitter.descendant_pump().expect("descendant pump token");
    assert!(!stop.is_stopped());

    subscriber.stop();
    subscriber.stop();
    assert!(stop.is_stopped());
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn dropping_subscriber_signals_descendant_pump() {
    let (emitter, subscriber) =
        ObserverEmitter::new(ObserverConfig::with_categories([EventCategory::Process]));
    let (_, stop) = emitter.descendant_pump().expect("descendant pump token");
    assert!(!stop.is_stopped());

    drop(subscriber);
    assert!(stop.is_stopped());
}

#[test]
fn system_wide_scope_preserves_phase3_reason_contract() {
    // The SystemWide matrix must keep matching the pre-#539 behavior so
    // downstream tests and clud UX don't regress.
    let caps = ObserverCapabilities::negotiate_for_scope(TraceScope::SystemWide);
    for category in [
        EventCategory::File,
        EventCategory::Network,
        EventCategory::Process,
    ] {
        let entry = caps.category(category);
        assert_eq!(entry.support, CapabilitySupport::Unavailable);
        assert!(
            entry.reason.contains("Phase 3"),
            "SystemWide reason must keep the Phase 3 anchor: {:?}",
            entry.reason
        );
    }
}

#[test]
fn render_summary_names_the_negotiated_scope() {
    let lpt =
        ObserverCapabilities::negotiate_for_scope(TraceScope::LaunchedProcessTree).render_summary();
    assert!(
        lpt.starts_with("observer capabilities (scope=launched-process-tree):\n"),
        "LaunchedProcessTree summary must lead with its scope: {lpt:?}"
    );
    let sw = ObserverCapabilities::negotiate_for_scope(TraceScope::SystemWide).render_summary();
    assert!(
        sw.starts_with("observer capabilities (scope=system-wide):\n"),
        "SystemWide summary must lead with its scope: {sw:?}"
    );
}
