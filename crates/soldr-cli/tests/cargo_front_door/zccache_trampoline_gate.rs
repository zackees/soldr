//! Gate tests for the Soldr-owned `soldr zccache` compatibility surface.

#![allow(clippy::print_stdout)]

use std::process::Output;

use serde_json::Value;

use crate::common;

struct EntryEnv {
    cache_dir: tempfile::TempDir,
    home_dir: tempfile::TempDir,
    namespace: String,
}

impl EntryEnv {
    fn new(tag: &str) -> Self {
        Self {
            cache_dir: tempfile::tempdir().expect("create temp cache dir"),
            home_dir: tempfile::tempdir().expect("create temp home dir"),
            namespace: format!("soldr-gate-{tag}-{}", std::process::id()),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_spec("zccache", args)
    }

    fn run_spec(&self, spec: &str, args: &[&str]) -> Output {
        common::isolated_soldr_command()
            .arg(spec)
            .args(args)
            // Keep every compatibility probe inside this test's Soldr-owned
            // root.  `ZCCACHE_CACHE_DIR` below is an inherited upstream
            // sentinel: the command must not adopt it as the daemon root.
            .env("SOLDR_CACHE_DIR", self.cache_dir.path())
            .env("HOME", self.home_dir.path())
            .env("USERPROFILE", self.home_dir.path())
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
    assert_eq!(output.status.code(), Some(2), "output: {text}");
    assert!(
        text.contains("embedded") && text.contains("soldr"),
        "output: {text}"
    );
    assert!(!text.contains("upstream"), "output: {text}");
}

#[test]
fn status_subcommand_is_refused() {
    let output = EntryEnv::new("status").run(&["status"]);
    let text = combined(&output);
    assert_eq!(output.status.code(), Some(2), "output: {text}");
    assert!(
        text.contains("embedded") && text.contains("soldr status"),
        "output: {text}"
    );
    assert!(!text.contains("fetch"), "output: {text}");
}

#[test]
fn cache_root_uses_soldr_owned_resolution() {
    let env = EntryEnv::new("cacheroot");
    let output = env.run(&["cache-root", "--json"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
    let json: Value = serde_json::from_str(&text).expect("cache-root JSON");
    assert_eq!(json["owner"], "soldr");
    let root = json["cache_root"].as_str().expect("cache root string");
    assert!(root.contains("zccache"), "root: {root}");
    assert_ne!(
        root,
        env.cache_dir.path().to_string_lossy(),
        "must ignore inherited ZCCACHE_CACHE_DIR"
    );
}

#[test]
fn session_end_id_json_routes_to_native_soldr_command() {
    let output = EntryEnv::new("sessionend").run(&[
        "session-end",
        "--id",
        "00000000-0000-0000-0000-000000000001",
        "--json",
    ]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
    let json: Value = serde_json::from_str(&text).expect("session-end JSON");
    assert_eq!(json["command"], "session-end");
    assert_eq!(json["session_id"], "00000000-0000-0000-0000-000000000001");
    assert_eq!(json["already_ended"], true);
    assert!(!text.contains("upstream"), "output: {text}");
}

#[test]
fn session_end_legacy_positional_id_remains_a_native_alias() {
    let output = EntryEnv::new("sessionend-positional")
        .run(&["session-end", "00000000-0000-0000-0000-000000000002"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
    assert!(
        text.contains("session-end: 00000000-0000-0000-0000-000000000002"),
        "output: {text}"
    );
    assert!(!text.contains("upstream"), "output: {text}");
}

#[test]
fn version_flag_is_served_by_soldr() {
    let output = EntryEnv::new("version").run(&["--version"]);
    let text = combined(&output);
    assert!(output.status.success(), "output: {text}");
}

#[test]
fn help_and_no_args_are_reserved_without_fetching() {
    let env = EntryEnv::new("help");
    let help = env.run(&["--help"]);
    let help_text = combined(&help);
    assert!(help.status.success(), "output: {help_text}");
    assert!(
        help_text.contains("compatibility commands"),
        "output: {help_text}"
    );

    let bare = env.run(&[]);
    let bare_text = combined(&bare);
    assert_eq!(bare.status.code(), Some(2), "output: {bare_text}");
    assert!(
        bare_text.contains("compatibility commands"),
        "output: {bare_text}"
    );
    assert!(!bare_text.contains("fetch"), "output: {bare_text}");
}

#[test]
fn version_selector_is_rejected_without_external_resolution() {
    let output = EntryEnv::new("selector").run_spec("zccache@1.2.3", &["cache-root"]);
    let text = combined(&output);
    assert_eq!(output.status.code(), Some(2), "output: {text}");
    assert!(
        text.contains("version selectors are unsupported"),
        "output: {text}"
    );
    assert!(text.contains("embedded in soldr"), "output: {text}");
    assert!(!text.contains("fetch"), "output: {text}");
}

#[test]
fn retired_and_unsupported_forms_have_migration_guidance() {
    for args in [
        ["rust-plan"].as_slice(),
        ["unknown-upstream-command"].as_slice(),
    ] {
        let output = EntryEnv::new("retired").run(args);
        let text = combined(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "args={args:?} output: {text}"
        );
        assert!(text.contains("soldr cargo"), "args={args:?} output: {text}");
        assert!(!text.contains("fetch"), "args={args:?} output: {text}");
    }
}

#[test]
fn stop_targets_soldr_daemon_and_leaves_no_upstream_runtime_copy() {
    let env = EntryEnv::new("nospawn");
    let stop = env.run(&["stop"]);
    let text = combined(&stop);
    assert!(stop.status.success(), "output: {text}");
    assert!(
        text.contains("soldr-daemon: not running") || text.contains("soldr-daemon: stopped"),
        "stop must report the Soldr daemon target, not an inherited upstream endpoint: {text}"
    );
    assert!(!text.contains("zccache-daemon"), "output: {text}");
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
