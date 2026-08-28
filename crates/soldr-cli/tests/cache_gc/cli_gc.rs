#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Wall-clock bound for the registry-touch convergence poll.
///
/// Paired with [`GC_TOUCH_MIN_POLLS`], and neither is sufficient alone,
/// because this loop's cost is not what it looks like: **every iteration
/// spawns a whole `soldr gc list` front door**, so the 100 ms sleep between
/// attempts is a rounding error next to process startup. On an idle host that
/// startup is ~60 ms (measured via `SOLDR_STARTUP_TRACE`, soldr#2571) and the
/// budget below buys dozens of observations; on a contended windows-2025
/// runner paying broker/daemon cold-start image hashing (soldr#2517) it buys a
/// handful — on exactly the runner where convergence is slowest.
///
/// That is the soldr#2624 family's whole shape: a fixed window colliding with
/// cold-start cost, failing on `main` where no PR can be blamed (run
/// 32052826733, `expected at least one tracked target dir within 10s`).
const GC_TOUCH_POLL_BUDGET: Duration = Duration::from_secs(10);

/// Floor on *observations*, not time. A runner slow enough that the budget
/// above buys two polls is precisely the runner that needs more than two
/// looks; failing there measures runner weather rather than the touch. Worst
/// case is this many polls times the per-poll cost, which stays well inside
/// the binary's 120 s nextest budget (the failing run's whole test was 42 s).
const GC_TOUCH_MIN_POLLS: usize = 8;

/// Pause between `gc list` polls.
///
/// Named because it is a third of the per-poll cost in the soldr#2785 sighting
/// (100ms of ~278ms), and a message reporting only "per poll" invites reading
/// all of it as work.
const GC_TOUCH_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn soldr_command(soldr_bin: &Path) -> Command {
    let mut command = Command::new(soldr_bin);
    common::scrub_outer_soldr_env(&mut command);
    command
}

#[test]
fn gc_summary_surfaces_the_linked_worktree_total() {
    // soldr#2134 wants merged-worktree targets reclaimed eagerly. Deleting
    // outside disk pressure is a wider change; surfacing the total is what
    // lets someone act before a build blocks, which is the same benefit
    // without widening what gets deleted.
    let cache_root = unique_temp_dir("gc-worktree-total");
    let plain = seed_gc_candidate(&cache_root, "primary-checkout");
    let worktree = seed_gc_worktree_candidate(&cache_root, "linked-worktree");

    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc");
    assert!(output.status.success());
    assert!(
        plain.exists() && worktree.exists(),
        "gc summary must not delete"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("in linked worktree"),
        "summary must surface the worktree total: {stdout}"
    );
    // Exactly one of the two seeded targets is a linked worktree; the plain
    // one holds no `.git` at all, so it must not be counted.
    assert!(
        stdout.contains("1 in linked worktree"),
        "only the worktree-backed target counts: {stdout}"
    );
    assert!(
        stdout.contains("[linked worktree]"),
        "and it is marked in the eviction-order list: {stdout}"
    );
}

#[test]
fn gc_summary_is_non_destructive_and_lists_largest_candidates() {
    let cache_root = unique_temp_dir("gc-summary");
    let target = seed_gc_candidate(&cache_root, "summary-project");

    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc");

    assert!(
        output.status.success(),
        "gc summary failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        target.exists(),
        "soldr gc summary must not delete {}",
        target.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reclaimable:"),
        "summary should include total reclaimable bytes: {stdout}"
    );
    assert!(
        stdout.contains("next to be reclaimed (eviction order)"),
        "summary should list candidates in the order purge will take them: {stdout}"
    );
    assert!(
        stdout.contains("Run 'soldr gc purge'"),
        "summary should point to destructive purge command: {stdout}"
    );
}

#[test]
fn gc_summary_json_reports_candidates_without_deleting() {
    let cache_root = unique_temp_dir("gc-summary-json");
    let target = seed_gc_candidate(&cache_root, "summary-json-project");

    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "--json", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc --json");

    assert!(
        output.status.success(),
        "gc summary json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        target.exists(),
        "soldr gc --json summary must not delete {}",
        target.display()
    );

    let json: Value = serde_json::from_slice(&output.stdout).expect("gc --json must be JSON");
    assert_eq!(json["schema_version"], 3);
    assert_eq!(json["command"], "gc");
    assert_eq!(json["mode"], "summary");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["candidate_count"], 1);
    assert!(json["total_reclaimable_bytes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(json["largest_candidates"].as_array().unwrap().len(), 1);
    assert_eq!(json["deleted_paths"].as_array().unwrap().len(), 0);
    let cand = &json["largest_candidates"].as_array().unwrap()[0];
    assert_eq!(cand["kind"].as_str(), Some("cargo_target"));
    assert_eq!(cand["purge_safety"].as_str(), Some("derived"));
}

#[test]
fn gc_purge_all_deletes_candidates_without_prompt() {
    let cache_root = unique_temp_dir("gc-purge-all");
    let target = seed_gc_candidate(&cache_root, "purge-project");

    let output = soldr_command(&common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--all",
            "--older-than",
            "1s",
            "--larger-than",
            "1B",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc purge --all");

    assert!(
        output.status.success(),
        "gc purge --all failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !target.exists(),
        "soldr gc purge --all should delete {}",
        target.display()
    );
}

#[test]
fn gc_purge_enter_accepts_candidate() {
    let cache_root = unique_temp_dir("gc-purge-enter");
    let target = seed_gc_candidate(&cache_root, "purge-enter-project");

    let mut child = soldr_command(&common::soldr_bin())
        .args(["gc", "purge", "--older-than", "1s", "--larger-than", "1B"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn soldr gc purge");
    child
        .stdin
        .as_mut()
        .expect("stdin should be piped")
        .write_all(b"\n")
        .expect("failed to accept prompt");
    let output = child
        .wait_with_output()
        .expect("failed to wait for soldr gc purge");

    assert!(
        output.status.success(),
        "gc purge interactive failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !target.exists(),
        "Enter should accept and delete {}",
        target.display()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[Y/n]"),
        "prompt should advertise default yes: {stderr}"
    );
    assert!(
        stderr.contains("selected 1; succeeded 1; failed 0"),
        "final summary should include aggregate counts: {stderr}"
    );
}

#[test]
fn gc_purge_all_json_reports_error_log_path_and_keeps_failed_row() {
    let cache_root = unique_temp_dir("gc-purge-json-failure");
    let target = seed_gc_file_candidate(&cache_root, "purge-json-failure-project");

    let output = soldr_command(&common::soldr_bin())
        .args([
            "gc",
            "purge",
            "--all",
            "--json",
            "--older-than",
            "1s",
            "--larger-than",
            "1B",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc purge --all --json");

    assert!(
        output.status.success(),
        "gc purge --all --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("gc purge --json must be JSON");
    assert_eq!(json["mode"], "purge");
    assert_eq!(json["selected_count"], 1);
    assert_eq!(json["succeeded_count"], 0);
    assert_eq!(json["failed_count"], 1);
    let log_path = PathBuf::from(
        json["error_log_path"]
            .as_str()
            .expect("failure JSON should include error log path"),
    );
    assert!(
        log_path.exists(),
        "missing gc error log {}",
        log_path.display()
    );

    let registry = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
        &cache_root.join("state.sqlite3"),
    )
    .expect("failed to open target registry");
    assert!(
        registry.get(&target).unwrap().is_some(),
        "failed deletion row should remain retryable"
    );
}

/// One `soldr front-door:` phase line, split into its name and its numbers.
///
/// `phase=<name> ms=<own> total_ms=<cumulative>` -- `ms` is that phase's own
/// duration, `total_ms` is everything up to and including it.
struct PhaseLine {
    name: String,
    own_ms: u64,
    total_ms: u64,
}

fn phase_lines(stderr: &str) -> Vec<PhaseLine> {
    stderr
        .lines()
        .filter(|line| line.contains("soldr front-door:"))
        .filter_map(|line| {
            let mut name = None;
            let mut own_ms = None;
            let mut total_ms = None;
            for token in line.split_whitespace() {
                if let Some(value) = token.strip_prefix("phase=") {
                    name = Some(value.to_string());
                } else if let Some(value) = token.strip_prefix("total_ms=") {
                    total_ms = value.parse::<u64>().ok();
                } else if let Some(value) = token.strip_prefix("ms=") {
                    own_ms = value.parse::<u64>().ok();
                }
            }
            Some(PhaseLine {
                name: name?,
                own_ms: own_ms?,
                total_ms: total_ms?,
            })
        })
        .collect()
}

/// How the child's traced time divides into *reaching* the command and
/// *running* it: `(startup_ms, command_body_ms)`.
///
/// soldr#2785 added the `command_dispatch` phase precisely so the trace would
/// not stop at `clap_parse` and leave the command body unaccounted for -- and
/// then read the cumulative `total_ms` of the *last* line, which is now that
/// same command body, and printed it as "in-process startup". Observed on
/// run 32897362298: `command_dispatch ms=238 total_ms=246` was reported as
/// "246ms was in-process startup", so the message sent the reader to
/// soldr#2624's contended-startup branch when startup was 8ms and the `gc
/// list` query itself was 238ms. The two diagnoses this arithmetic exists to
/// separate were being merged back together by it.
///
/// Startup is therefore the largest cumulative total among the phases that are
/// *not* the command body. The body is its own `ms=`. A trace from a binary
/// without the phase yields `None` for the body rather than silently folding
/// it in.
fn front_door_split(stderr: &str) -> Option<(u64, Option<u64>)> {
    let lines = phase_lines(stderr);
    if lines.is_empty() {
        return None;
    }
    const BODY: &str = "command_dispatch";
    let body_ms = lines
        .iter()
        .find(|line| line.name == BODY)
        .map(|line| line.own_ms);
    let startup_ms = lines
        .iter()
        .filter(|line| line.name != BODY)
        .map(|line| line.total_ms)
        .max()
        .unwrap_or(0);
    Some((startup_ms, body_ms))
}

/// The `(startup, body)` split rendered for the assertion message.
fn front_door_cost_summary(stderr: &str) -> String {
    match front_door_split(stderr) {
        Some((startup_ms, Some(body_ms))) => format!(
            "of which {startup_ms}ms was in-process startup and {body_ms}ms \
             was the `gc list` command body"
        ),
        Some((startup_ms, None)) => format!(
            "of which {startup_ms}ms was in-process startup (the trace has no \
             `command_dispatch` phase, so the command body is unaccounted for)"
        ),
        None => "with no front-door trace (soldr exited before the front door)".to_string(),
    }
}

/// The front-door phase lines from a captured poll's stderr (soldr#2624).
///
/// Only the `soldr front-door:` lines matter here; a contended runner also
/// emits broker warnings, bootstrap notices and cache chatter, and burying the
/// timing breakdown in that is how the two diagnoses stayed indistinguishable.
/// Bounded, because a panic message is read in a CI log: the tail is kept
/// rather than the head, since the phases that dominate a slow start
/// (`broker_image_hash`, `broker_spawn_wait`) come last.
fn startup_trace_tail(stderr: &str) -> String {
    const MAX_LINES: usize = 20;
    let phases: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("soldr front-door:"))
        .collect();
    if phases.is_empty() {
        return "(no front-door trace lines — soldr exited before the front door)".to_string();
    }
    let skipped = phases.len().saturating_sub(MAX_LINES);
    let mut rendered = String::new();
    if skipped > 0 {
        rendered.push_str(&format!("... {skipped} earlier phase line(s) omitted\n"));
    }
    rendered.push_str(&phases[skipped..].join("\n"));
    rendered
}

/// The daemon's own account of the touch's write half (soldr#2785).
///
/// The front-door trace above says whether *startup* ate the budget. It
/// cannot say anything about the write, because the write is fire-and-forget
/// — the client is acked before the store is touched and never learns the
/// outcome. The daemon does log both failures (`target-touch dropped` after
/// its 2s open-retry budget, `target-touch upsert failed` on a write error),
/// but into its own stderr log, which no assertion here was reading. So a
/// missing row and a dropped touch looked identical from the test.
///
/// Best-effort by construction: this runs inside a panic message on a lane
/// that is already failing, so an unreadable or absent log reports itself
/// rather than replacing the real failure with an I/O error.
fn daemon_target_touch_lines(cache_root: &Path) -> String {
    let log = cache_root.join("daemon-spawn.log");
    let Ok(text) = fs::read_to_string(&log) else {
        return format!("(no readable daemon log at {})", log.display());
    };
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("target-touch"))
        .collect();
    if lines.is_empty() {
        return format!(
            "(none in {} — the daemon logged no drop and no upsert failure)",
            log.display()
        );
    }
    lines.join("\n")
}

#[test]
fn gc_list_json_reports_built_project_target_dir() {
    let cache_root = unique_temp_dir("gc-list-build");
    let home_root = unique_temp_dir("gc-list-build-home");
    let project_dir = unique_temp_dir("gc-list-project");
    // #323 slice 2: sandbox CARGO_HOME so the registry_src walker
    // doesn't see the developer's real `~/.cargo` and inject
    // cargo_registry_src entries into this test's assertions. Also
    // sandbox RUSTUP_HOME now that `gc list` reports rustup toolchains.
    let sandbox_cargo_home = unique_temp_dir("gc-list-build-cargo-home");
    let sandbox_rustup_home = unique_temp_dir("gc-list-build-rustup-home");

    fs::write(
        project_dir.join("Cargo.toml"),
        "[package]\nname = \"gc_list_demo\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("failed to write Cargo.toml");
    fs::create_dir_all(project_dir.join("src")).expect("failed to create src dir");
    fs::write(project_dir.join("src/main.rs"), "fn main() {}\n").expect("failed to write main.rs");

    let soldr_bin = common::soldr_bin();
    let cargo = rustup_which("cargo");
    let daemon = common::isolated_daemon::IsolatedDaemon::spawn(
        &common::soldr_daemon_bin(),
        &cache_root,
        &home_root,
    );

    // soldr#2785: run from the fixture project, not the inherited cwd.
    // These tests execute with cargo's cwd inside this checkout, and the
    // workspace manifest sets `[workspace.metadata.soldr] prefer_newer_global
    // = true`. `global_upgrade::maybe_delegate` walks ancestors for that flag
    // and, on a hit, runs `<global soldr> --version` as a CHILD PROCESS -- per
    // its own doc, a released soldr's front door "stages a broker image under
    // the inherited HOME and spawns `broker serve` before it prints its
    // version", which is what made the broker-absent tests find a broker in
    // their isolated homes (soldr#2521 D).
    //
    // So every invocation here was paying a process spawn, and the poll loop
    // below was paying one per iteration: `global_upgrade` dominates the
    // front-door trace in all three recorded failures (143ms, 201ms, 271ms of
    // totals 151/209/280). The fixture project lives under the OS temp dir,
    // whose ancestors carry no such manifest, so the gate is false and the
    // probe never runs.
    let mut build_command = Command::new(&cargo);
    daemon.configure_client(&mut build_command);
    let build = build_command
        .args(["build", "--quiet"])
        .current_dir(&project_dir)
        .env("RUSTC_WRAPPER", &soldr_bin)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove(soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR)
        .env("ZCCACHE_DISABLE", "1")
        .env("SOLDR_CACHE_ENABLED", "0")
        .env_remove("CARGO_TARGET_DIR")
        // This fixture intentionally exercises soldr as a plain
        // RUSTC_WRAPPER, not as a child of the outer `soldr cargo test`
        // session that may be running this integration test.
        .env_remove(soldr_cli::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .env_remove(soldr_cli::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR)
        // Cache-disable keeps this registry fixture on direct rustc after the
        // wrapper records its target directory.
        .env_remove("ZCCACHE_BINARY")
        .output()
        .expect("failed to run cargo build through soldr wrapper");

    assert!(
        build.status.success(),
        "cargo build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let target_dir = project_dir.join("target");
    assert!(target_dir.exists(), "target/ should exist after build");

    let canonical_target = fs::canonicalize(&target_dir).unwrap_or_else(|_| target_dir.clone());

    // The wrapper's RecordTargetTouch is fire-and-forget by design: the
    // daemon acks receipt BEFORE persisting (soldr#2561's hot-path
    // contract), so the build returning does not imply the registry row
    // is readable yet. Poll until the touch converges instead of racing
    // it -- a single-shot read flaked the Linux x64 lane at exactly this
    // assertion once suite timing shifted (soldr#2575's run).
    let poll_started = Instant::now();
    let deadline = poll_started + GC_TOUCH_POLL_BUDGET;
    let mut attempts = 0usize;
    // Assigned on every iteration before the assertion can read it, so no
    // initializer -- an initial value here would be dead.
    let mut last_poll_trace: String;
    // Wall time of the child alone, so the assertion can separate it from the
    // deliberate sleep between polls (soldr#2785). No initializer, for the
    // same reason `last_poll_trace` has none.
    let mut last_child_wall: Duration;
    let json: Value = loop {
        attempts += 1;
        let child_started = Instant::now();
        let mut gc_command = soldr_command(&soldr_bin);
        daemon.configure_client(&mut gc_command);
        let output = gc_command
            .current_dir(&project_dir)
            .args(["gc", "list", "--json"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("HOME", &home_root)
            .env("USERPROFILE", &home_root)
            .env_remove(soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR)
            .env("CARGO_HOME", &sandbox_cargo_home)
            .env("RUSTUP_HOME", &sandbox_rustup_home)
            // soldr#2624: the assertion below offers two diagnoses and could
            // not tell them apart. This trace is what separates them — it
            // breaks the poll's wall time into front-door phases. Free on the
            // passing path: the test asserts on stdout, and this only writes
            // stderr, which is captured either way.
            .env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1")
            .output()
            .expect("failed to run soldr gc list --json");
        last_child_wall = child_started.elapsed();
        last_poll_trace = String::from_utf8_lossy(&output.stderr).into_owned();

        assert!(
            output.status.success(),
            "gc list --json failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let json: Value =
            serde_json::from_slice(&output.stdout).expect("gc list --json must be JSON");
        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["command"], "gc");
        assert_eq!(json["mode"], "list");
        let entry_count = json["entry_count"].as_u64().expect("entry_count");
        if entry_count >= 1 {
            break json;
        }
        let elapsed = poll_started.elapsed();
        assert!(
            attempts < GC_TOUCH_MIN_POLLS || Instant::now() < deadline,
            "no tracked target dir after {attempts} `gc list` polls over \
             {elapsed:?} (~{:?} per poll; budget {GC_TOUCH_POLL_BUDGET:?}, \
             floor {GC_TOUCH_MIN_POLLS} polls).\n\n\
             Where the last poll went: {:?} wall in the child, {}, and \
             {GC_TOUCH_POLL_INTERVAL:?} sleeping between polls. Startup \
             dominating means a contended runner spending the budget on \
             process startup (soldr#2624); the command body dominating means \
             startup was fine and `gc list` itself is slow; both being small \
             means the poll was cheap and the registry row genuinely never \
             landed (soldr#2561). Whatever the three do not account for is \
             process creation, which the front-door trace cannot cover \
             because it begins after the process is already running.\n\n\
             soldr#2785: if the row never landed, the daemon already says why \
             — the touch's write half logs `target-touch dropped` when the \
             store cannot be opened within its 2s retry budget, and \
             `target-touch upsert failed` when the write itself errors. \
             Silence there means the touch was never delivered or the row was \
             written and `gc list` cannot see it, which are different \
             bugs.\n\ndaemon target-touch lines:\n{}\n\nlast poll trace:\n{}",
            elapsed / u32::try_from(attempts).unwrap_or(1),
            last_child_wall,
            front_door_cost_summary(&last_poll_trace),
            daemon_target_touch_lines(&cache_root),
            startup_trace_tail(&last_poll_trace),
        );
        std::thread::sleep(GC_TOUCH_POLL_INTERVAL);
    };

    let entries = json["entries"].as_array().expect("entries array");
    let canonical_str = canonical_target.display().to_string();
    let target_str = target_dir.display().to_string();
    let matched = entries.iter().find(|e| {
        let p = e["path"].as_str().unwrap_or("");
        let pb = PathBuf::from(p);
        if p == canonical_str || p == target_str || pb == canonical_target || pb == target_dir {
            return true;
        }
        if let Ok(canon_p) = fs::canonicalize(&pb) {
            return canon_p == canonical_target;
        }
        false
    });
    let entry = matched.unwrap_or_else(|| {
        panic!(
            "target dir {} not found in gc list entries: {}",
            canonical_target.display(),
            serde_json::to_string_pretty(entries).unwrap()
        )
    });

    let path_str = entry["path"].as_str().expect("entry path");
    assert_eq!(entry["kind"].as_str(), Some("cargo_target"));
    assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
    assert!(
        PathBuf::from(path_str).is_absolute(),
        "gc list entries must use absolute paths: {path_str}"
    );
    let size_bytes = entry["size_bytes"].as_u64().expect("size_bytes");
    assert!(size_bytes > 0, "built target/ should have non-zero size");
    let file_count = entry["file_count"].as_u64().expect("file_count");
    assert!(file_count > 0, "built target/ should contain files");

    for entry in entries {
        assert!(
            entry["path"].is_string(),
            "every entry must have a path string"
        );
        assert!(
            entry["size_bytes"].is_u64(),
            "every entry must have size_bytes"
        );
        assert!(
            entry["file_count"].is_u64(),
            "every entry must have file_count"
        );
        assert!(
            entry.get("exists").is_none(),
            "missing rows are pruned, so `exists` should not appear on listed entries"
        );
        let kind = entry["kind"].as_str().expect("every entry must have kind");
        assert!(
            matches!(
                kind,
                "cargo_target"
                    | "cargo_target_incremental"
                    | "cargo_target_build_script_binaries"
                    | "cargo_target_doc"
                    | "cargo_target_subcommand_caches"
            ),
            "unexpected gc list kind: {kind}"
        );
        assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
    }

    let stop = soldr_command(&soldr_bin)
        .current_dir(&project_dir)
        .args(["daemon", "stop"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("stop explicit daemon");
    assert!(
        stop.status.success(),
        "daemon stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
}

#[test]
fn gc_list_json_entries_include_kind_and_purge_safety_defaults() {
    let cache_root = unique_temp_dir("gc-list-kind-defaults");
    let target = seed_gc_candidate(&cache_root, "kind-defaults-project");

    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr gc list --json");
    assert!(output.status.success());

    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema_version"], 3);

    let entries = json["entries"].as_array().expect("entries");
    let entry = entries
        .iter()
        .find(|e| e["path"].as_str().is_some_and(|p| target == Path::new(p)))
        .expect("seeded target missing from entries");
    assert_eq!(entry["kind"].as_str(), Some("cargo_target"));
    assert_eq!(entry["purge_safety"].as_str(), Some("derived"));
    // soldr#2134: eviction ranks worktree targets ahead of primary
    // checkouts, so the report has to say which one an entry is or the
    // resulting order is unexplainable. A plain seeded workspace is not
    // a linked worktree.
    assert_eq!(
        entry["in_worktree"].as_bool(),
        Some(false),
        "cargo_target entries must carry the tier that decides their eviction order"
    );
}

#[test]
fn gc_list_json_prunes_missing_registry_rows_in_one_pass() {
    let cache_root = unique_temp_dir("gc-list-prune");
    let dev_root = cache_root.join("dev-root");
    // #323 slice 2: sandbox CARGO_HOME so the registry_src walker
    // doesn't contribute extra entries to entry_count assertions. Also
    // sandbox RUSTUP_HOME now that `gc list` reports rustup toolchains.
    let sandbox_cargo_home = unique_temp_dir("gc-list-prune-cargo-home");
    let sandbox_rustup_home = unique_temp_dir("gc-list-prune-rustup-home");

    let live_workspace = dev_root.join("live-project");
    let live_target = live_workspace.join("target");
    fs::create_dir_all(&live_target).expect("failed to create live target dir");
    fs::write(live_target.join("artifact.bin"), b"keep me").expect("failed to seed live target");

    let missing_target = dev_root.join("ghost-project").join("target");

    {
        let registry = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
            &cache_root.join("state.sqlite3"),
        )
        .expect("failed to open target registry");
        let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
            .expect("failed to read clock for seeding");
        registry
            .upsert_with_time(&live_target, now - 30)
            .expect("failed to seed live row");
        registry
            .upsert_with_time(&missing_target, now - 30)
            .expect("failed to seed missing row");
        assert!(
            registry.get(&missing_target).unwrap().is_some(),
            "missing-row seed precondition"
        );
    }

    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "list", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("CARGO_HOME", &sandbox_cargo_home)
        .env("RUSTUP_HOME", &sandbox_rustup_home)
        .output()
        .expect("failed to run soldr gc list --json");
    assert!(
        output.status.success(),
        "gc list --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("gc list must be JSON");
    assert_eq!(json["mode"], "list");
    assert_eq!(json["entry_count"], 1);
    assert_eq!(json["pruned_missing"], 1);

    let entries = json["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1, "missing entry must not be reported");
    let missing_str = missing_target.display().to_string();
    for entry in entries {
        let p = entry["path"].as_str().unwrap_or("");
        assert_ne!(p, missing_str, "missing path leaked into output");
    }

    let registry_after = soldr_cli::cache_lib::target_registry::TargetRegistry::open(
        &cache_root.join("state.sqlite3"),
    )
    .expect("failed to reopen registry");
    assert!(
        registry_after.get(&missing_target).unwrap().is_none(),
        "missing row should be batched out of the registry"
    );
    assert!(
        registry_after.get(&live_target).unwrap().is_some(),
        "live row must be preserved"
    );
}

#[test]
fn gc_flat_all_is_rejected_with_purge_hint() {
    let output = soldr_command(&common::soldr_bin())
        .args(["gc", "--all"])
        .env("SOLDR_CACHE_DIR", unique_temp_dir("gc-flat-all"))
        .output()
        .expect("failed to run soldr gc --all");

    assert!(
        !output.status.success(),
        "legacy flat gc --all must not silently delete"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("soldr gc purge --all"),
        "error should point to the new purge command: {stderr}"
    );
}

/// The exact trace from run 32897362298's msvc failure -- the one the old
/// arithmetic reported as "246ms was in-process startup".
const OBSERVED_TRACE: &str = "\
soldr front-door: startup phase=reentrancy_guard ms=0 total_ms=0
soldr front-door: startup phase=clap_parse ms=3 total_ms=8
soldr front-door: startup phase=command_dispatch ms=238 total_ms=246
";

#[test]
fn the_command_body_is_not_counted_as_startup() {
    // The regression this exists for: `command_dispatch` was added by
    // soldr#2785 so the command body would stop hiding, and then the
    // cumulative total of that very line was printed as startup. On the run
    // above that turned 8ms of startup into "246ms", which is the difference
    // between soldr#2624's branch and soldr#2561's.
    let (startup_ms, body_ms) = front_door_split(OBSERVED_TRACE).expect("a trace was present");
    assert_eq!(startup_ms, 8);
    assert_eq!(body_ms, Some(238));
}

#[test]
fn the_summary_names_both_halves_separately() {
    let summary = front_door_cost_summary(OBSERVED_TRACE);
    assert!(summary.contains("8ms was in-process startup"), "{summary}");
    assert!(
        summary.contains("238ms was the `gc list` command body"),
        "{summary}"
    );
    // The old message would have said this, and it was the wrong number.
    assert!(
        !summary.contains("246ms was in-process startup"),
        "{summary}"
    );
}

#[test]
fn a_trace_without_the_body_phase_says_so_rather_than_folding_it_in() {
    // An older soldr, or a process that died mid-command, emits no
    // `command_dispatch`. Reporting its startup as if it covered the whole
    // poll is exactly the error being fixed, so the gap is named instead.
    let trace = "soldr front-door: startup phase=clap_parse ms=3 total_ms=8\n";
    assert_eq!(front_door_split(trace), Some((8, None)));
    let summary = front_door_cost_summary(trace);
    assert!(summary.contains("8ms was in-process startup"), "{summary}");
    assert!(summary.contains("unaccounted for"), "{summary}");
}

#[test]
fn a_slow_startup_phase_still_dominates_when_it_should() {
    // The other branch has to keep working: a genuinely contended start
    // (soldr#2624) must still read as startup-dominated.
    let trace = "\
soldr front-door: startup phase=broker_spawn_wait ms=900 total_ms=910
soldr front-door: startup phase=command_dispatch ms=4 total_ms=914
";
    assert_eq!(front_door_split(trace), Some((910, Some(4))));
}

#[test]
fn non_front_door_noise_is_ignored() {
    // A contended runner interleaves broker warnings and cache chatter; a
    // parser that picked up `ms=` from those would report a fabricated split.
    let trace = "\
soldr wrapper: startup phase=command_dispatch ms=999 total_ms=999
soldr front-door: startup phase=clap_parse ms=3 total_ms=8
random line with no numbers
soldr front-door: startup phase=command_dispatch ms=238 total_ms=246
";
    assert_eq!(front_door_split(trace), Some((8, Some(238))));
}

#[test]
fn no_trace_at_all_is_distinguished_from_a_zero_cost_trace() {
    assert_eq!(front_door_split("nothing here\n"), None);
    assert!(front_door_cost_summary("nothing here\n").contains("no front-door trace"));
}
