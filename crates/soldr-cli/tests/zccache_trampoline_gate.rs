//! Gate tests for the Soldr-owned `soldr zccache` compatibility surface.

#![allow(clippy::print_stdout)]

use std::process::{Command, Output};

mod common;

struct EntryEnv {
    cache_dir: tempfile::TempDir,
    namespace: String,
}

impl EntryEnv {
    fn new(tag: &str) -> Self {
        Self {
            cache_dir: tempfile::tempdir().expect("create temp cache dir"),
            namespace: format!("soldr-gate-{tag}-{}", std::process::id()),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(common::soldr_bin())
            .arg("zccache")
            .args(args)
            .env("ZCCACHE_CACHE_DIR", self.cache_dir.path())
            .env("ZCCACHE_DAEMON_NAMESPACE", &self.namespace)
            .output()
            .expect("run Soldr-owned zccache compatibility command")
    }
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn spawning_subcommand_is_refused_with_embedded_hint() {
    let env = EntryEnv::new("start");
    let output = env.run(&["start"]);
    let text = combined(&output);
    assert!(!output.status.success(), "output: {text}");
    assert!(
        text.contains("embedded") && text.contains("soldr"),
        "output: {text}"
    );
}

#[test]
fn status_subcommand_is_refused() {
    let output = EntryEnv::new("status").run(&["status"]);
    let text = combined(&output);
    assert!(!output.status.success(), "output: {text}");
    assert!(text.contains("embedded"), "output: {text}");
}

#[test]
fn cache_root_uses_soldr_owned_resolution() {
    let output = EntryEnv::new("cacheroot").run(&["cache-root", "--json"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
}

#[test]
fn session_end_routes_to_native_soldr_command() {
    let output =
        EntryEnv::new("sessionend").run(&["session-end", "00000000-0000-0000-0000-000000000001"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
}

#[test]
fn version_flag_is_served_by_soldr() {
    let output = EntryEnv::new("version").run(&["--version"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
}

#[test]
fn no_spawn_guard_leaves_no_daemon_runtime_copy() {
    let env = EntryEnv::new("nospawn");
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
            } else {
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(!name.starts_with("zccache-daemon"), "found {path:?}");
            }
        }
    }
}
