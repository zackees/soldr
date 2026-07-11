//! Gate tests for the compiled-in `zccache` CLI trampoline (soldr#1467).
//!
//! soldr's build cache runs as an embedded service inside soldr-daemon;
//! a standalone `zccache-daemon` process must never spawn. The trampoline
//! (`src/zccache_entry.rs`, selected by the `zccache` argv[0] alias)
//! therefore only passes through daemon-free subcommands and hard-errors
//! the rest, pointing at the embedded equivalents. It also exports
//! `ZCCACHE_NO_SPAWN=1` (zccache#982) as defense-in-depth so even an
//! allowlisted subcommand can never lazily spawn a daemon.
//!
//! Tests run the real trampoline binary with an isolated cache dir +
//! daemon namespace, so even the RED state of this test (no gate yet)
//! cannot touch a real daemon.

#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use soldr_cli::timed_test;
mod common;

/// Materialize the trampoline alias from the one compiled soldr binary.
fn trampoline_bin() -> Option<PathBuf> {
    let alias = common::zccache_bin();
    alias.exists().then_some(alias)
}

/// Isolated invocation env: unique cache root + daemon namespace so an
/// accidental spawn (RED state) is contained and stoppable.
struct TrampolineEnv {
    bin: PathBuf,
    cache_dir: tempfile::TempDir,
    namespace: String,
}

impl TrampolineEnv {
    /// `None` when the trampoline binary is unavailable (target runner
    /// replaying an archive without it) — the test skips cleanly, the
    /// gate contract stays covered on the native build lanes.
    fn new(tag: &str) -> Option<Self> {
        let Some(bin) = trampoline_bin() else {
            eprintln!("skipping {tag}: zccache trampoline binary unavailable in this environment");
            return None;
        };
        Some(Self {
            bin,
            cache_dir: tempfile::tempdir().expect("create temp cache dir"),
            namespace: format!("soldr-gate-{tag}-{}", std::process::id()),
        })
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(&self.bin)
            .args(args)
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", &self.namespace)
            .output()
            .expect("run zccache trampoline")
    }
}

impl Drop for TrampolineEnv {
    /// Best-effort: if a RED run really spawned an isolated daemon,
    /// stop it so a failing test cannot leak a process.
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .arg("stop")
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", &self.namespace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

timed_test!(spawning_subcommand_is_refused_with_embedded_hint, {
    let Some(env) = TrampolineEnv::new("start") else {
        return;
    };
    let output = env.run(&["start"]);
    let text = combined(&output);
    assert!(
        !output.status.success(),
        "`zccache start` must be refused by the trampoline gate.\noutput: {text}"
    );
    assert!(
        text.contains("embedded"),
        "refusal must explain the embedded-service architecture.\noutput: {text}"
    );
    assert!(
        text.contains("soldr"),
        "refusal must point back at the soldr surface.\noutput: {text}"
    );
});

timed_test!(status_subcommand_is_refused, {
    let Some(env) = TrampolineEnv::new("status") else {
        return;
    };
    let output = env.run(&["status"]);
    let text = combined(&output);
    assert!(
        !output.status.success(),
        "`zccache status` must be refused (embedded equivalent: `soldr status`).\noutput: {text}"
    );
    assert!(text.contains("embedded"), "output: {text}");
});

timed_test!(cache_root_passes_through, {
    let Some(env) = TrampolineEnv::new("cacheroot") else {
        return;
    };
    let output = env.run(&["cache-root"]);
    let text = combined(&output);
    assert!(
        output.status.success(),
        "`zccache cache-root` is daemon-free and must pass through.\noutput: {text}"
    );
});

timed_test!(session_end_passes_through, {
    let Some(env) = TrampolineEnv::new("sessionend") else {
        return;
    };
    // Daemon-unreachable `session-end` is idempotent exit-0 (zccache#150);
    // the id is arbitrary — no daemon exists in this isolated namespace.
    let output = env.run(&["session-end", "soldr-gate-test-session"]);
    let text = combined(&output);
    assert!(
        output.status.success(),
        "`zccache session-end <id>` is daemon-free (idempotent, zccache#150) and must pass through.\noutput: {text}"
    );
});

timed_test!(version_flag_passes_through, {
    let Some(env) = TrampolineEnv::new("version") else {
        return;
    };
    let output = env.run(&["--version"]);
    let text = combined(&output);
    assert!(
        output.status.success(),
        "`zccache --version` has no subcommand and must pass through.\noutput: {text}"
    );
});

timed_test!(
    no_spawn_guard_env_reaches_the_vendored_cli,
    Duration::from_secs(240),
    {
        // `start` is refused by the allowlist; `stop` is allowlisted and
        // must still see ZCCACHE_NO_SPAWN=1 exported by the trampoline.
        // `zccache stop` with no daemon prints a "not running"-style
        // message and exits 0 without spawning — the observable contract
        // here is simply that nothing was spawned and no runtime-binaries
        // copy appeared in the isolated cache root.
        let Some(env) = TrampolineEnv::new("nospawn") else {
            return;
        };
        let _ = env.run(&["stop"]);
        let mut stack = vec![env.cache_dir.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.starts_with("zccache-daemon"),
                    "found daemon runtime copy {path:?} under the isolated cache root"
                );
            }
        }
    }
);
