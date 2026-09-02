//! Reproduction test for the daemon RSS ceiling watchdog (soldr#3038).
//!
//! The motivating observation: a canonical `soldr-daemon` on production
//! hardware reached 11.7 GiB of private anonymous memory over ~90 minutes of
//! continuous building. This test cannot reproduce that scale inside a
//! nextest budget -- it drives a small, hermetic, dependency-chained
//! workspace through a real daemon instead of a multi-hour production
//! fleet -- so it is not a reproduction of the 11.7 GiB growth. What it *is*
//! a real regression gate for: the owner's stated target, "the daemon should
//! operate below half a gigabyte of RAM" (`CEILING_BYTES` below), exercised
//! against a real `soldr-daemon` process serving a real (if small) compile
//! workload end to end -- daemon spawn, wrapped `rustc` invocations routed
//! through the embedded zccache service, and an in-process RSS sample taken
//! by the daemon about itself.
//!
//! Uses the daemon's own `SOLDR_DAEMON_RSS_CEILING_BYTES` watchdog
//! (`soldr_cli::daemon::rss_ceiling`) rather than sampling RSS from outside
//! the process: an external poller can miss a spike between samples, and a
//! breach here must produce a legible assertion message (daemon pid,
//! ceiling, observed RSS) rather than a bare "connection refused" from a
//! daemon that killed itself mid-build -- see that module's docs for why the
//! watchdog only ever records a breach and never exits.
//!
//! `SOLDR_*` variables are forwarded wholesale into a broker-spawned daemon
//! (`crate::daemon::lifecycle::spawn_env::FORWARDED_ENV_PREFIX`), so setting
//! `SOLDR_DAEMON_RSS_CEILING_BYTES` on the first `soldr cargo build` in this
//! test is enough to arm the watchdog in the daemon it spawns -- no direct
//! daemon control plumbing is needed here, unlike the IPC-query fixtures in
//! `cli_daemon_builds.rs`.

#![allow(clippy::print_stdout)]

use crate::common;
use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::rss_ceiling::{self, RssCeilingStatus};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

/// The owner's stated target steady state (soldr#3038): "the daemon should
/// operate below half a gigabyte of RAM."
const CEILING_BYTES: u64 = 512 * 1024 * 1024;

/// Chained workspace crates. Each depends on the previous by path, so the
/// build is inherently sequential (real dependency-graph work, not just
/// parallel fan-out) and every crate is a distinct `rustc` invocation routed
/// through the wrapper -- the unit soldr's cache and the daemon's in-memory
/// state actually key on.
const CRATE_COUNT: usize = 12;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn crate_name(index: usize) -> String {
    format!("wk{index:02}")
}

/// Writes a small virtual workspace of `CRATE_COUNT` library crates chained
/// by path dependency (`wk01` depends on `wk00`, ...). No external
/// dependencies and no network access: the workload must be hermetic so the
/// test is not flaky on an offline or sandboxed runner. Returns the
/// workspace root.
fn write_workload_workspace(root: &Path) -> PathBuf {
    let project = root.join("workload");
    fs::create_dir_all(&project).expect("create workload root");
    let members: String = (0..CRATE_COUNT)
        .map(|i| format!("    \"{}\",\n", crate_name(i)))
        .collect();
    fs::write(
        project.join("Cargo.toml"),
        format!("[workspace]\nresolver = \"2\"\nmembers = [\n{members}]\n"),
    )
    .expect("write workspace Cargo.toml");

    for index in 0..CRATE_COUNT {
        write_workload_crate(&project, index, 0);
    }
    project
}

/// (Re)writes one chained crate's `Cargo.toml` + `src/lib.rs`. `extra_fns`
/// lets a rebuild pass append fresh functions to force a real content-hash
/// change (a cache miss, not a no-op rebuild) without touching every crate.
fn write_workload_crate(project: &Path, index: usize, extra_fns: usize) {
    let name = crate_name(index);
    let dir = project.join(&name);
    fs::create_dir_all(dir.join("src")).expect("create crate src dir");
    let dep = if index == 0 {
        String::new()
    } else {
        format!(
            "{} = {{ path = \"../{}\" }}\n",
            crate_name(index - 1),
            crate_name(index - 1)
        )
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n{dep}"
        ),
    )
    .expect("write crate Cargo.toml");

    let mut body = String::new();
    for fn_index in 0..20 + extra_fns {
        body.push_str(&format!(
            "pub fn f{fn_index}(x: u64) -> u64 {{ x.wrapping_mul({fn_index_plus}).wrapping_add(1) }}\n",
            fn_index_plus = fn_index as u64 + 1
        ));
    }
    if index > 0 {
        let prev = crate_name(index - 1);
        body.push_str(&format!(
            "pub fn chained(x: u64) -> u64 {{ {prev}::f0(x).wrapping_add(f0(x)) }}\n"
        ));
    }
    fs::write(dir.join("src").join("lib.rs"), body).expect("write crate lib.rs");
}

fn daemon_bin() -> PathBuf {
    common::soldr_daemon_bin()
}

/// Same shape as `cli_daemon_lifecycle.rs::run_soldr_with_timeout`: a
/// broker-mediated `soldr cargo ...` invocation (not the direct-control IPC
/// pattern `cli_daemon_builds.rs` uses for non-compile queries), because
/// driving a real compile through the wrapper needs the front door's normal
/// daemon discovery/spawn path -- which is exactly the path that forwards
/// `SOLDR_DAEMON_RSS_CEILING_BYTES` into the daemon it spawns.
fn run_soldr_build(
    args: &[&str],
    cache_root: &Path,
    home_root: &Path,
    current_dir: &Path,
    ceiling_bytes: u64,
    timeout: Duration,
) -> std::process::Output {
    let mut cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
    cmd.args(args)
        .current_dir(current_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .env(
            soldr_cli::daemon::lifecycle::SOLDR_DAEMON_EXE_ENV_VAR,
            common::isolated_daemon::isolated_daemon_executable(&daemon_bin(), cache_root),
        )
        .env(rss_ceiling::RSS_CEILING_ENV_VAR, ceiling_bytes.to_string())
        .env("SOLDR_DAEMON_SPAWN_RETRY_BUDGET_MS", "10000");

    let mut child = cmd.spawn().expect("spawn soldr");
    if child
        .wait_timeout(timeout)
        .expect("wait for soldr")
        .is_none()
    {
        let _ = child.kill();
        let output = child.wait_with_output().expect("collect timed-out output");
        panic!(
            "soldr {args:?} timed out after {timeout:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let output = child.wait_with_output().expect("collect soldr output");
    assert!(
        output.status.success(),
        "soldr {args:?} did not exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stop_daemon(cache_root: &Path, home_root: &Path) {
    let mut cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut cmd);
    let _ = cmd
        .args(["daemon", "stop"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .output();
    let mut broker_cmd = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut broker_cmd);
    let _ = broker_cmd
        .args(["broker", "stop"])
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("HOME", home_root)
        .env("USERPROFILE", home_root)
        .output();
}

struct DaemonCleanup {
    cache_root: PathBuf,
    home_root: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        stop_daemon(&self.cache_root, &self.home_root);
    }
}

/// Poll the watchdog status file for up to `deadline`. The watchdog samples
/// every [`rss_ceiling::RSS_SAMPLE_INTERVAL`] (2s) and only starts writing
/// once the ceiling env var was seen at daemon spawn, so the file may not
/// exist for the first couple of seconds after the daemon comes up.
fn wait_for_status(paths: &SoldrPaths, deadline: Instant) -> RssCeilingStatus {
    loop {
        if let Some(status) = rss_ceiling::read_status(paths) {
            if status.sample_count > 0 {
                return status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "soldr-daemon RSS ceiling watchdog never wrote a status file with a sample \
             (soldr#3038): the daemon may not have forwarded SOLDR_DAEMON_RSS_CEILING_BYTES, \
             or never sampled before the deadline"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn daemon_stays_under_the_512mib_target_while_serving_a_chained_build() {
    let cache_root = unique_temp_dir("rss-ceiling-cache");
    let home_root = unique_temp_dir("rss-ceiling-home");
    let cleanup = DaemonCleanup {
        cache_root: cache_root.clone(),
        home_root: home_root.clone(),
    };

    let project = write_workload_workspace(&cache_root);

    // Pass 1: cold build of the whole chain -- CRATE_COUNT distinct rustc
    // invocations, sequential because each crate depends on the last. This
    // is also what spawns the daemon and forwards the ceiling env var into
    // it (see `run_soldr_build`'s doc comment).
    run_soldr_build(
        &["cargo", "build", "--quiet"],
        &cache_root,
        &home_root,
        &project,
        CEILING_BYTES,
        Duration::from_secs(180),
    );

    // Passes 2-4: touch a crate partway down the chain and rebuild. Each
    // pass is a genuine content-hash change (new functions appended, not a
    // no-op touch), so it is a real cache miss + recompile of that crate and
    // everything downstream of it, not a warm no-op that skips the wrapper
    // entirely.
    for (pass, touch_index) in [
        (1, CRATE_COUNT / 3),
        (2, CRATE_COUNT / 2),
        (3, CRATE_COUNT - 1),
    ] {
        write_workload_crate(&project, touch_index, pass * 5);
        run_soldr_build(
            &["cargo", "build", "--quiet"],
            &cache_root,
            &home_root,
            &project,
            CEILING_BYTES,
            Duration::from_secs(120),
        );
    }

    let paths = SoldrPaths::with_root(cache_root.clone());
    // The workload above is done; give the watchdog one more sample window
    // past the workload so a peak reached late in the last build is not
    // missed by racing the file write.
    let status = wait_for_status(&paths, Instant::now() + Duration::from_secs(15));

    // Report the observed peak regardless of pass/fail (soldr#3038 asks for
    // this explicitly): a passing run should still show the headroom under
    // the 512 MiB target, and this workload's small scale relative to the
    // 11.7 GiB production observation is itself a finding worth recording.
    println!(
        "soldr#3038 daemon RSS ceiling reproduction: pid={} ceiling={ceiling_mib:.1}MiB \
         peak={peak_mib:.1}MiB last={last_mib:.1}MiB samples={} mimalloc_heap_committed={:?} \
         mimalloc_detailed_stats={}",
        status.pid,
        status.sample_count,
        status.mimalloc_heap_committed_bytes,
        status.mimalloc_stats_detailed,
        ceiling_mib = status.ceiling_bytes as f64 / (1024.0 * 1024.0),
        peak_mib = status.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        last_mib = status.last_rss_bytes as f64 / (1024.0 * 1024.0),
    );

    assert_eq!(status.ceiling_bytes, CEILING_BYTES);
    assert!(
        !status.breached,
        "soldr-daemon (pid {}) exceeded the {:.1} MiB RSS ceiling: peak observed {:.1} MiB \
         across {} samples -- see rss-ceiling-v1.json under {}",
        status.pid,
        status.ceiling_bytes as f64 / (1024.0 * 1024.0),
        status.peak_rss_bytes as f64 / (1024.0 * 1024.0),
        status.sample_count,
        paths.cache.join("soldr-daemon").display(),
    );

    drop(cleanup);
}
