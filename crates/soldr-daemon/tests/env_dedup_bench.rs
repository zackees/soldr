//! Measurement harness for issue #1550 — "perf(ipc): deduplicate
//! compile-request environment serialization".
//!
//! NOT a perf regression gate. This is the measure-first evidence run
//! behind the issue's >=1% CPU/wall acceptance gate: it times the
//! env-proportional parts of the `Request::Compile` build / encode /
//! decode path at 50 / 500 / 5000 env vars and compares the current
//! full-env shape against a session-dictionary prototype shape
//! (per-unit volatile subset sent verbatim + 32-byte content hash of
//! the stable remainder, daemon reconstructing the full env from a
//! bounded dictionary).
//!
//! Default run uses small iteration counts so the suite stays fast;
//! for the real numbers run in release with the full-iteration knob:
//!
//! ```text
//! SOLDR_ENV_DEDUP_BENCH_FULL=1 \
//!   cargo test --release -p soldr-daemon --test env_dedup_bench -- --nocapture
//! ```
//!
//! ## Recorded verdict (2026-07-10, Linux container, 4 cores, release)
//!
//! Per-compile env-path CPU (wrapper clone + wire-convert/encode +
//! daemon decode), current vs prototype (prototype still pays a sha256
//! of the stable partition every compile + a daemon-side dictionary
//! clone to rebuild the owned env Vec zccache's API requires):
//!
//! | synth vars | current | prototype | saved   | wire bytes        |
//! |------------|---------|-----------|---------|-------------------|
//! | 50         | 59.1us  | 37.4us    | 21.8us  | 10176 -> 3406     |
//! | 500        | 246.1us | 142.9us   | 103.2us | 43477 -> 3406     |
//! | 5000       | 5.96ms  | 1.73ms    | 4.22ms  | 376477 -> 3406    |
//!
//! Against a measured all-cache-hit 20-unit rebuild wall (2.3s
//! baseline / ~2.4s at +500 vars / ~4.9s at +5000 vars) the savable
//! share is 0.02% / 0.08% / ~1.7% — under the 1% gate for ordinary and
//! representative CI. The dominant env-proportional cost lives inside
//! the embedded zccache compile itself (hit dispatch p50 1.6ms at
//! baseline -> 19.1ms at +5000 vars), which a wire-level dictionary
//! cannot address because the daemon must still materialize the full
//! owned env for the zccache API. Hypothesis disproven; see issue
//! #1550 for the full evidence trail.

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use soldr_daemon::daemon::protocol::{CompileLifecycle, CompileRequest, Request};
use soldr_daemon::daemon::wire::{decode_request, encode_request};

/// Synthetic environment: a realistic PATH-sized entry, a handful of
/// ordinary session vars, plus `n` CI-injected vars of realistic size
/// (key ~16 chars, value ~50 chars).
fn synth_env(n: usize) -> Vec<(String, String)> {
    let mut env = Vec::with_capacity(n + 6);
    env.push(("PATH".to_string(), "x".repeat(3000)));
    env.push(("HOME".to_string(), "/home/ci-runner".to_string()));
    env.push((
        "RUSTUP_HOME".to_string(),
        "/home/ci-runner/.rustup".to_string(),
    ));
    env.push((
        "CARGO_HOME".to_string(),
        "/home/ci-runner/.cargo".to_string(),
    ));
    env.push(("SHELL".to_string(), "/bin/bash".to_string()));
    env.push(("TERM".to_string(), "xterm-256color".to_string()));
    for i in 0..n {
        env.push((
            format!("SYNTH_CI_VAR_{i:05}"),
            format!("value-{i:05}-{}", "v".repeat(38)),
        ));
    }
    env
}

/// The per-unit volatile partition a dedup design would always send
/// verbatim (CARGO_PKG_*, OUT_DIR, CARGO_CRATE_NAME, dylib path...).
/// ~45 entries, matching what cargo sets on each rustc invocation.
fn volatile_env() -> Vec<(String, String)> {
    let mut env = Vec::with_capacity(48);
    for i in 0..40 {
        env.push((
            format!("CARGO_PKG_SYNTH_{i:02}"),
            format!("per-unit-value-{i:02}"),
        ));
    }
    env.push((
        "OUT_DIR".to_string(),
        "/repo/target/debug/build/some-crate-0123456789abcdef/out".to_string(),
    ));
    env.push((
        "CARGO_MANIFEST_DIR".to_string(),
        "/repo/crates/some-crate".to_string(),
    ));
    env.push(("CARGO_CRATE_NAME".to_string(), "some_crate".to_string()));
    env.push((
        "LD_LIBRARY_PATH".to_string(),
        "/repo/target/debug/deps:/home/ci-runner/.rustup/toolchains/x/lib".to_string(),
    ));
    env
}

/// A realistic cargo-generated rustc argv (~45 args, ~1.4 KB).
fn synth_args() -> Vec<String> {
    let mut args = vec![
        "/home/ci-runner/.rustup/toolchains/1.94.1-x86_64-unknown-linux-gnu/bin/rustc".to_string(),
        "--crate-name".to_string(),
        "some_crate".to_string(),
        "--edition=2021".to_string(),
        "crates/some-crate/src/lib.rs".to_string(),
        "--error-format=json".to_string(),
        "--json=diagnostic-rendered-ansi,artifacts,future-incompat".to_string(),
        "--crate-type".to_string(),
        "lib".to_string(),
        "--emit=dep-info,metadata,link".to_string(),
        "-C".to_string(),
        "embed-bitcode=no".to_string(),
        "-C".to_string(),
        "debuginfo=2".to_string(),
        "--out-dir".to_string(),
        "/repo/target/debug/deps".to_string(),
        "-L".to_string(),
        "dependency=/repo/target/debug/deps".to_string(),
    ];
    for i in 0..14 {
        args.push("--extern".to_string());
        args.push(format!(
            "dep{i}=/repo/target/debug/deps/libdep{i}-0123456789abcdef.rmeta"
        ));
    }
    args
}

fn build_request(env: Vec<(String, String)>) -> CompileRequest {
    CompileRequest {
        args: synth_args(),
        cwd: "/repo".to_string(),
        env,
        stdin: Vec::new(),
        lifecycle: Some(CompileLifecycle {
            session_id: 42,
            crate_name: "some_crate".to_string(),
            target_dir: "/repo/target".to_string(),
            started_at_ms: 1_752_000_000_000,
        }),
        ipc_busy_retries: 0,
    }
}

fn time_per_iter<F: FnMut()>(iters: usize, mut f: F) -> Duration {
    // Warmup.
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed() / iters as u32
}

fn hash_env(env: &[(String, String)]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (k, v) in env {
        hasher.update(k.as_bytes());
        hasher.update([0u8]);
        hasher.update(v.as_bytes());
        hasher.update([0u8]);
    }
    hasher.finalize().into()
}

fn full_mode() -> bool {
    std::env::var_os("SOLDR_ENV_DEDUP_BENCH_FULL").is_some()
}

#[test]
fn env_dedup_micro_benchmark() {
    println!();
    println!("issue #1550 env-dedup micro-benchmark (per-compile costs)");
    if full_mode() {
        println!("full mode: iteration counts sized for stable release numbers");
    } else {
        println!("smoke mode: tiny iteration counts (set SOLDR_ENV_DEDUP_BENCH_FULL=1 + --release for real numbers)");
    }
    println!();

    for &n in &[50usize, 500, 5000] {
        let iters = match (full_mode(), n) {
            (true, 50) => 4000,
            (true, 500) => 1000,
            (true, _) => 200,
            (false, 50) => 100,
            (false, 500) => 25,
            (false, _) => 5,
        };

        // ---- current shape: full env rides every request ----
        let full_env = synth_env(n);
        // per-unit volatile vars ride too (they're part of the real env)
        let mut cur_env = full_env.clone();
        cur_env.extend(volatile_env());
        let cur_req = build_request(cur_env.clone());

        let t_clone_cur = time_per_iter(iters, || {
            let c = cur_req.clone();
            std::hint::black_box(&c);
        });
        let wire_req = Request::Compile(cur_req.clone());
        let bytes_cur = encode_request(&wire_req);
        let t_encode_cur = time_per_iter(iters, || {
            let b = encode_request(&wire_req);
            std::hint::black_box(&b);
        });
        let t_decode_cur = time_per_iter(iters, || {
            let r = decode_request(&bytes_cur).expect("decode");
            std::hint::black_box(&r);
        });

        // ---- prototype shape: volatile verbatim + 32-byte hash ----
        // Wrapper still must hash the stable partition every compile.
        let t_hash = time_per_iter(iters, || {
            let h = hash_env(&full_env);
            std::hint::black_box(&h);
        });
        let mut proto_env = volatile_env();
        // Approximate the wire cost of the hash field with one more
        // env entry carrying the hex hash (~66 bytes ≈ optional bytes
        // field + tag overhead).
        proto_env.push((
            "SOLDR_ENV_DICT_HASH".to_string(),
            hex::encode(hash_env(&full_env)),
        ));
        let proto_req = build_request(proto_env);
        let t_clone_proto = time_per_iter(iters, || {
            let c = proto_req.clone();
            std::hint::black_box(&c);
        });
        let wire_proto = Request::Compile(proto_req.clone());
        let bytes_proto = encode_request(&wire_proto);
        let t_encode_proto = time_per_iter(iters, || {
            let b = encode_request(&wire_proto);
            std::hint::black_box(&b);
        });
        let t_decode_proto = time_per_iter(iters, || {
            let r = decode_request(&bytes_proto).expect("decode");
            std::hint::black_box(&r);
        });
        // Daemon must still materialize the full env for the compile
        // service from its dictionary (owned Vec<(String,String)>).
        let t_dict_clone = time_per_iter(iters, || {
            let c = cur_env.clone();
            std::hint::black_box(&c);
        });

        let cur_total = t_clone_cur + t_encode_cur + t_decode_cur;
        let proto_total = t_hash + t_clone_proto + t_encode_proto + t_decode_proto + t_dict_clone;

        println!("== n = {n} synthetic env vars (+6 session +45 volatile) ==");
        println!(
            "  current  : clone {:>9.2?}  encode {:>9.2?}  decode {:>9.2?}  bytes {:>8}",
            t_clone_cur,
            t_encode_cur,
            t_decode_cur,
            bytes_cur.len()
        );
        println!(
            "  prototype: clone {:>9.2?}  encode {:>9.2?}  decode {:>9.2?}  bytes {:>8}",
            t_clone_proto,
            t_encode_proto,
            t_decode_proto,
            bytes_proto.len()
        );
        println!(
            "             hash {:>9.2?}  dict-clone {:>9.2?}",
            t_hash, t_dict_clone
        );
        println!(
            "  per-compile CPU: current {:>9.2?} vs prototype {:>9.2?}  -> saved {:>9.2?}",
            cur_total,
            proto_total,
            cur_total.saturating_sub(proto_total)
        );
        println!(
            "  wire bytes: {} -> {} (saved {})",
            bytes_cur.len(),
            bytes_proto.len(),
            bytes_cur.len() as i64 - bytes_proto.len() as i64
        );
        println!();
    }
}
