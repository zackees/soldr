//! Tests for `SpawnCommandRequest::clear_inherited_env` — the daemon's
//! equivalent of Python's `subprocess.Popen(env=…)` replace semantic.
//!
//! Two modes:
//!
//! | clear_inherited_env | Subprocess sees                         |
//! |---------------------|-----------------------------------------|
//! | `false` (default)   | <daemon inherited env> ∪ <caller env>   |
//! | `true`              | only <caller env>                       |
//!
//! Subject of the probes is a small Rust testbin (`testbin-env-dump`)
//! that writes its full env to a file in argv[1]. We invoke it via
//! the daemon's shell wrapper, then parse the file back to check what
//! the subprocess actually saw.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use running_process::daemon::client::{DaemonClient, SpawnCommandRequest};

use super::{scaled, start_server_with_tempdb};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Locate the env-dump testbin. Builds it on demand.
fn env_dump_path() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "testbins",
            "--bin",
            "testbin-env-dump",
            "--message-format=json",
        ])
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("cargo build");
    assert!(output.status.success(), "cargo build failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains("testbin-env-dump") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v["target"]["kind"]
                .as_array()
                .is_some_and(|a| a.iter().any(|k| k == "bin"))
            {
                if let Some(exe) = v["executable"].as_str() {
                    let p = PathBuf::from(exe);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !p.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    assert!(p.exists(), "env-dump bin not found at {p:?}");
                    return p;
                }
            }
        }
    }
    panic!("env-dump executable not in cargo output");
}

/// Parse a `KEY=VALUE\n` file into a HashMap. Wait briefly for it
/// to appear since the daemon's spawned process is async.
fn read_env_file(path: &Path) -> HashMap<String, String> {
    let deadline = std::time::Instant::now() + scaled(std::time::Duration::from_secs(5));
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read env dump file {path:?}: {e}"));
    let mut map = HashMap::new();
    for line in contents.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

/// Shell-quote a path for cmd.exe (Windows) or sh (Unix). Both accept
/// the path inside double quotes for our purposes — paths from
/// tempfile::tempdir don't contain backslashes that need escaping
/// beyond the outer quoting.
fn shell_quote_path(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// **Default (inherit).** With `clear_inherited_env=false` (the
/// default), the subprocess sees the daemon's inherited env layered
/// with anything the caller adds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_inherits_daemon_env_and_layers_caller_env() {
    let scope = format!("envrep-inherit-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let dump_bin = env_dump_path();
    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("env.dump");

    // Build the shell command: invoke env-dump with the output path.
    let command = format!("{} {}", shell_quote_path(&dump_bin), shell_quote_path(&out));

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let req = SpawnCommandRequest::shell(command).with_env("RP_TEST_LAYERED", "from-caller");
        let _ = client.spawn_command(&req).expect("spawn_command");

        let env_map = read_env_file(&out_for_client);
        assert_eq!(
            env_map.get("RP_TEST_LAYERED").map(String::as_str),
            Some("from-caller"),
            "caller-supplied env var must reach the subprocess"
        );
        // The daemon inherits the runtime's PATH, so the subprocess
        // should see one too (this is what makes shell invocations
        // work).
        let has_path_like = env_map.contains_key("PATH") || env_map.contains_key("Path");
        assert!(
            has_path_like,
            "subprocess should inherit a PATH/Path var via the daemon, env was: {env_map:?}"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **Replace.** With `clear_inherited_env=true`, the subprocess sees
/// only what the caller supplied. The caller is responsible for
/// including platform essentials (e.g. SystemRoot on Windows for
/// cmd.exe to load DLLs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_mode_subprocess_sees_only_caller_env() {
    let scope = format!("envrep-replace-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let dump_bin = env_dump_path();
    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("env.dump");

    let command = format!("{} {}", shell_quote_path(&dump_bin), shell_quote_path(&out));

    // Caller-supplied env: only the platform essentials the shell
    // wrapper actually needs + our probe var.
    let mut caller_env: Vec<(String, String)> =
        vec![("RP_TEST_REPLACED".to_string(), "from-replace".to_string())];
    if cfg!(windows) {
        // cmd.exe needs SystemRoot to load DLLs. Copy it from the
        // test's own env so it matches whatever the runner uses.
        if let Ok(root) = std::env::var("SystemRoot") {
            caller_env.push(("SystemRoot".to_string(), root));
        }
        // PATH so cmd.exe can find the binary by absolute path
        // (which it can without PATH actually, but include it for
        // realism since most callers will).
        if let Ok(path) = std::env::var("PATH") {
            caller_env.push(("PATH".to_string(), path));
        }
    } else {
        if let Ok(path) = std::env::var("PATH") {
            caller_env.push(("PATH".to_string(), path));
        }
    }

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let req = SpawnCommandRequest::shell(command).with_env_replace(caller_env.clone());
        assert!(req.clear_inherited_env, "builder must set the clear flag");

        let _ = client.spawn_command(&req).expect("spawn_command");

        let env_map = read_env_file(&out_for_client);

        // 1) Caller's probe var IS visible.
        assert_eq!(
            env_map.get("RP_TEST_REPLACED").map(String::as_str),
            Some("from-replace"),
            "caller env must reach the subprocess in replace mode"
        );

        // 2) Caller-supplied SystemRoot / PATH ARE visible (we put
        //    them in the replace map ourselves).
        if cfg!(windows) {
            assert!(
                env_map.contains_key("SystemRoot"),
                "caller-provided SystemRoot must reach subprocess (got {env_map:?})"
            );
        }

        // 3) Critically: env vars that exist in the DAEMON's env but
        //    were NOT in the caller's map must NOT appear in the
        //    subprocess's env. We pick a marker var that we set on
        //    the daemon side but not in the caller map.
        //
        //    There's no way to mutate the daemon's env from this test
        //    (the daemon is a different process), but the daemon
        //    inherits THIS test process's env when it starts. So a
        //    var we set HERE will be in the daemon's env and would
        //    leak through in inherit mode.
        //
        //    Sentinel check: did we accidentally inherit the daemon
        //    user's HOME / USERPROFILE? If so the replace mode isn't
        //    doing its job.
        if cfg!(unix) {
            // Many CI runners have HOME set; if replace mode worked
            // the subprocess wouldn't see it (we didn't put it in
            // caller_env).
            assert!(
                !env_map.contains_key("HOME"),
                "replace mode leaked HOME from daemon env: {env_map:?}"
            );
        } else {
            // Windows equivalent — USERPROFILE or USERNAME would leak
            // through inherit mode.
            assert!(
                !env_map.contains_key("USERPROFILE"),
                "replace mode leaked USERPROFILE from daemon env: {env_map:?}"
            );
        }

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **Caller env wins ties under default (layer) mode.** When a key
/// exists in both the inherited env AND the caller's map, the
/// subprocess sees the caller's value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn layer_mode_caller_env_wins_ties_against_inherited() {
    let scope = format!("envrep-layer-tie-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let dump_bin = env_dump_path();
    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("env.dump");

    let command = format!("{} {}", shell_quote_path(&dump_bin), shell_quote_path(&out));

    // PATH almost certainly exists in the daemon's inherited env.
    // We override it with a deterministic value via the caller env.
    let caller_path_override = if cfg!(windows) {
        "C:\\caller-supplied-path-override"
    } else {
        "/caller-supplied-path-override"
    };

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let path_override = caller_path_override.to_string();
    let task = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        let path_key = if cfg!(windows) { "Path" } else { "PATH" };
        let req = SpawnCommandRequest::shell(command).with_env(path_key, path_override.clone());
        let _ = client.spawn_command(&req).expect("spawn_command");

        let env_map = read_env_file(&out_for_client);
        let observed = env_map
            .get(path_key)
            .or_else(|| env_map.get("PATH"))
            .or_else(|| env_map.get("Path"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            observed, path_override,
            "caller's env override must beat the inherited value"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}

/// **Windows case-insensitive override regression.** Drives the dual-key
/// scenario directly: feed `with_envs` an explicit list that contains BOTH
/// `("PATH", inherited_marker)` and `("Path", override_marker)`, then assert
/// the subprocess sees the override.
///
/// Without the daemon-side case-insensitive dedup, this is flaky because
/// the protobuf wire format used to be `map<string,string>` (unordered) and
/// the daemon would feed both entries to Rust's `Command::env` whose
/// `EnvKey` collapses them case-insensitively with last-write-wins —
/// HashMap iteration order then decides which one survives. With the fix
/// the daemon dedups on the receiving end before handing off to
/// `Command::envs`, so the LAST entry per case-folded key always wins.
///
/// Windows-only because Unix env names are case-sensitive and this race
/// can't exist there.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_case_insensitive_override_beats_inherited_path() {
    let scope = format!("envrep-winci-{}", line!());
    let (server_handle, socket, _tmp_dir) = start_server_with_tempdb(&scope);
    tokio::time::sleep(scaled(std::time::Duration::from_millis(300))).await;

    let dump_bin = env_dump_path();
    let workdir = tempfile::tempdir().expect("tempdir");
    let out = workdir.path().join("env.dump");

    let command = format!("{} {}", shell_quote_path(&dump_bin), shell_quote_path(&out));

    let inherited_marker = "C:\\should-not-win-marker";
    let override_marker = "C:\\override-marker";

    let socket_for_client = socket.clone();
    let out_for_client = out.clone();
    let task = tokio::task::spawn_blocking(move || {
        let mut client = DaemonClient::connect_to(&socket_for_client).expect("connect");
        // Explicit env list with both case variants, override last. We
        // bypass `with_env`'s case-sensitive lookup by going through
        // `with_envs` directly. `clear_inherited_env` is left default
        // (false) so the daemon ALSO layers its own env on top, but our
        // explicit `Path` should still beat both the inherited
        // daemon-side `Path` and the explicit `PATH` we put in.
        let req = SpawnCommandRequest::shell(command).with_envs([
            ("PATH".to_string(), inherited_marker.to_string()),
            ("Path".to_string(), override_marker.to_string()),
            // Include cmd.exe essentials so the shell invocation runs.
            (
                "SystemRoot".to_string(),
                std::env::var("SystemRoot").unwrap_or_default(),
            ),
        ]);
        let _ = client.spawn_command(&req).expect("spawn_command");

        let env_map = read_env_file(&out_for_client);
        let observed = env_map
            .get("Path")
            .or_else(|| env_map.get("PATH"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            observed, override_marker,
            "caller's last-listed override must beat the earlier case variant; got env: {env_map:?}"
        );

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    task.expect("client task");

    let _ = tokio::time::timeout(scaled(std::time::Duration::from_secs(5)), server_handle).await;
}
