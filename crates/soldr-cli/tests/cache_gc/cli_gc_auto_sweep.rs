//! Issue #1286 (F5): regression coverage for the auto-GC starvation fix.
//!
//! Before the fix, the auto-GC orchestrator was kicked at build START —
//! microseconds after `build_active::set(true)` — so every tick
//! deferred with `reason=build_active` and the sweep never executed on
//! machines that only invoke soldr for builds (observed locally: days
//! of continuous deferrals and a 36 GB cache). The fix spawns a
//! detached `soldr gc auto-sweep` process at build END, and the sweep
//! itself now records an `auto-gc status=run stage=start` line so
//! "ran but found nothing" and "never ran" are distinguishable.

use std::process::Command;

use crate::common;

#[test]
fn gc_auto_sweep_runs_and_logs_start_line() {
    let cache_root = tempfile::tempdir().expect("cache tempdir");
    let home_root = tempfile::tempdir().expect("home tempdir");

    let out = Command::new(common::soldr_bin())
        .args(["gc", "auto-sweep"])
        .env("SOLDR_CACHE_DIR", cache_root.path())
        .env("HOME", home_root.path())
        .env("USERPROFILE", home_root.path())
        .env_remove("RUSTC_WRAPPER")
        .output()
        .expect("run soldr gc auto-sweep");
    assert!(
        out.status.success(),
        "auto-sweep must exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let log = cache_root.path().join("logs").join("auto-gc.log");
    let content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        content.contains("status=run stage=start"),
        "auto-gc.log must record that the sweep STARTED (not just \
         deferrals); log content: {content:?}"
    );
    assert!(
        !content.contains("reason=build_active"),
        "a standalone auto-sweep process must not consider a build \
         active; log content: {content:?}"
    );
}
