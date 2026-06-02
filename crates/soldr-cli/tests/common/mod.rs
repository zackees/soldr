//! Shared helpers for the `cli_*` integration test binaries. Each binary
//! pulls in `mod common;` and re-exports the helpers it actually uses; the
//! `#![allow(dead_code)]` on the module silences unused-helper warnings on a
//! per-binary basis without sprinkling allows over individual helpers.
#![allow(dead_code)]

use serde_json::Value;
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn isolated_soldr_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_soldr"));
    scrub_outer_soldr_env(&mut command);
    command
}

pub(crate) fn scrub_outer_soldr_env(command: &mut Command) -> &mut Command {
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("SOLDR_LINKER")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS");
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("CARGO_TARGET_"))
        {
            command.env_remove(name);
        }
    }
    command
}

pub(crate) fn rustup_which(tool: &str) -> String {
    let output = Command::new("rustup")
        .args(["which", tool])
        .output()
        .expect("failed to resolve tool with rustup");
    assert!(output.status.success(), "rustup which failed for {tool}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let counter = TEMP_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let process_id = std::process::id();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{process_id}-{counter}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

pub(crate) fn toml_string(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

pub(crate) fn seed_gc_candidate(cache_root: &Path, label: &str) -> PathBuf {
    let dev_root = cache_root.join("dev-root");
    let workspace = dev_root.join(label);
    let target = workspace.join("target");
    fs::create_dir_all(&target).expect("failed to create target dir");
    fs::write(target.join("artifact.bin"), b"reclaim me").expect("failed to seed target file");
    fs::write(
        cache_root.join("config.toml"),
        format!("[gc]\nallowlist_roots = [\"{}\"]\n", toml_string(&dev_root)),
    )
    .expect("failed to write gc config");

    let registry =
        soldr_cli::cache_lib::target_registry::TargetRegistry::open(&cache_root.join("state.redb"))
            .expect("failed to open target registry");
    let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
        .expect("failed to get current unix seconds");
    registry
        .upsert_with_time(&target, now - 120)
        .expect("failed to seed target registry");
    target
}

pub(crate) fn seed_gc_file_candidate(cache_root: &Path, label: &str) -> PathBuf {
    let dev_root = cache_root.join("dev-root");
    let workspace = dev_root.join(label);
    fs::create_dir_all(&workspace).expect("failed to create workspace dir");
    let target = workspace.join("target");
    fs::write(&target, b"not a directory").expect("failed to seed target file");
    fs::write(
        cache_root.join("config.toml"),
        format!("[gc]\nallowlist_roots = [\"{}\"]\n", toml_string(&dev_root)),
    )
    .expect("failed to write gc config");

    let registry =
        soldr_cli::cache_lib::target_registry::TargetRegistry::open(&cache_root.join("state.redb"))
            .expect("failed to open target registry");
    let now = soldr_cli::cache_lib::target_registry::current_unix_seconds()
        .expect("failed to get current unix seconds");
    registry
        .upsert_with_time(&target, now - 120)
        .expect("failed to seed target registry");
    target
}

pub(crate) fn path_display_variants(path: &Path) -> Vec<String> {
    let mut variants = vec![path.display().to_string()];
    if let Ok(canonical) = fs::canonicalize(path) {
        let canonical = canonical.display().to_string();
        if !variants.contains(&canonical) {
            variants.push(canonical);
        }
    }
    variants
}

pub(crate) fn discovered_private_zccache_cache_dir(cache_root: &Path) -> PathBuf {
    let private_root = cache_root.join("cache").join("zccache").join("private");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&private_root)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", private_root.display()))
        .map(|entry| entry.expect("read private zccache dir").path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert_eq!(
        dirs.len(),
        1,
        "expected one private zccache cache dir under {}, found {:?}",
        private_root.display(),
        dirs
    );
    dirs.remove(0)
}

pub(crate) fn logged_cargo_wrapper(log: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let wrapper = line.strip_prefix("cargo wrapper=")?;
        wrapper
            .split_once(" rustc=")
            .map(|(wrapper, _)| wrapper.to_string())
    })
}

pub(crate) fn log_contains_owned_soldr_wrapper(log: &str, cache_root: &Path) -> bool {
    if log.contains(env!("CARGO_BIN_EXE_soldr")) {
        return true;
    }

    #[cfg(windows)]
    {
        let Some(wrapper) = logged_cargo_wrapper(log) else {
            return false;
        };
        let runtime_root = cache_root.join("runtime").join("soldr-self");
        path_display_variants(&runtime_root)
            .iter()
            .any(|path| wrapper.contains(path))
    }
    #[cfg(not(windows))]
    {
        let _ = cache_root;
        false
    }
}

pub(crate) fn log_contains_toolchain_homes(
    log: &str,
    prefix: &str,
    cargo_home: &Path,
    rustup_home: &Path,
) -> bool {
    for cargo_home in path_display_variants(cargo_home) {
        for rustup_home in path_display_variants(rustup_home) {
            if log.contains(&format!(
                "{prefix} cargo_home={cargo_home} rustup_home={rustup_home}"
            )) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join(format!("{name}.cmd"))
    }
    #[cfg(not(windows))]
    {
        dir.join(name)
    }
}

pub(crate) fn write_fake_script(path: &Path, body: &str) {
    #[cfg(windows)]
    {
        fs::write(path, body.replace('\n', "\r\n")).expect("failed to write fake script");
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).expect("failed to write fake script");
        let mut perms = fs::metadata(path)
            .expect("failed to stat fake script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("failed to chmod fake script");
    }
}

pub(crate) fn fake_cargo_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             if \"%~1\"==\"metadata\" (\n\
               if defined SOLDR_TEST_CARGO_METADATA_PATH (\n\
                 type \"%SOLDR_TEST_CARGO_METADATA_PATH%\"\n\
               ) else (\n\
                 echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% sccache_dir=%SCCACHE_DIR% zccache_dir=%ZCCACHE_CACHE_DIR% path_remap=%ZCCACHE_PATH_REMAP% worktree_root=%ZCCACHE_WORKTREE_ROOT%>>\"{0}\"\n\
                 for /f \"tokens=1,* delims==\" %%A in ('set CARGO_TARGET_ 2^>nul') do @echo cargo_target_env %%A=%%B>>\"{0}\"\n\
                 for /f \"tokens=1,* delims==\" %%A in ('set CARGO_PROFILE_ 2^>nul') do @echo cargo_profile_env %%A=%%B>>\"{0}\"\n\
                 echo {{}}\n\
               )\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"--version\" (\n\
               echo cargo version>>\"{0}\"\n\
               echo cargo 1.0.0-test\n\
               exit /b 0\n\
             )\n\
             echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% sccache_dir=%SCCACHE_DIR% zccache_dir=%ZCCACHE_CACHE_DIR% path_remap=%ZCCACHE_PATH_REMAP% worktree_root=%ZCCACHE_WORKTREE_ROOT%>>\"{0}\"\n\
             for /f \"tokens=1,* delims==\" %%A in ('set CARGO_TARGET_ 2^>nul') do @echo cargo_target_env %%A=%%B>>\"{0}\"\n\
             for /f \"tokens=1,* delims==\" %%A in ('set CARGO_PROFILE_ 2^>nul') do @echo cargo_profile_env %%A=%%B>>\"{0}\"\n\
             if defined RUSTC_WRAPPER (\n\
             call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name demo --emit dep-info,link\n\
             ) else (\n\
             call \"%RUSTC%\" --crate-name demo --emit dep-info,link\n\
             )\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             log_cargo_target_envs() {{\n\
               env | grep '^CARGO_TARGET_' | while IFS= read -r line; do\n\
                 echo \"cargo_target_env $line\" >> \"{0}\"\n\
               done\n\
             }}\n\
             log_cargo_profile_envs() {{\n\
               env | grep '^CARGO_PROFILE_' | while IFS= read -r line; do\n\
                 echo \"cargo_profile_env $line\" >> \"{0}\"\n\
               done\n\
             }}\n\
             if [ \"$1\" = \"metadata\" ]; then\n\
               if [ -n \"${{SOLDR_TEST_CARGO_METADATA_PATH:-}}\" ]; then\n\
                 cat \"$SOLDR_TEST_CARGO_METADATA_PATH\"\n\
               else\n\
                 echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} sccache_dir=${{SCCACHE_DIR:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}}\" >> \"{0}\"\n\
                 log_cargo_target_envs\n\
                 log_cargo_profile_envs\n\
                 echo '{{}}'\n\
               fi\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo 'cargo version' >> \"{0}\"\n\
               echo 'cargo 1.0.0-test'\n\
               exit 0\n\
             fi\n\
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} sccache_dir=${{SCCACHE_DIR:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}}\" >> \"{0}\"\n\
             log_cargo_target_envs\n\
             log_cargo_profile_envs\n\
             if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
               \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name demo --emit dep-info,link\n\
             else\n\
               \"$RUSTC\" --crate-name demo --emit dep-info,link\n\
             fi\n",
            log_path.display()
        )
    }
}

#[cfg(not(windows))]
pub(crate) fn fake_cargo_with_jobserver_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{}\"\n\
         exec 3</dev/null\n\
         exec 4>/dev/null\n\
         export CARGO_MAKEFLAGS='-j --jobserver-fds=3,4'\n\
         export SOLDR_TEST_JOBSERVER_READ_FD=3\n\
         export SOLDR_TEST_JOBSERVER_WRITE_FD=4\n\
         \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name demo --emit dep-info,link\n",
        log_path.display()
    )
}

pub(crate) fn fake_rustc_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             if \"%~1\"==\"-Vv\" (\n\
               echo rustc 1.0.0-test\n\
               echo host: x86_64-pc-windows-msvc\n\
               echo release: 1.0.0-test\n\
               exit /b 0\n\
             )\n\
             echo rustc %*>>\"{}\"\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-Vv\" ]; then\n\
               echo 'rustc 1.0.0-test'\n\
               echo 'host: x86_64-unknown-linux-gnu'\n\
               echo 'release: 1.0.0-test'\n\
               exit 0\n\
             fi\n\
             echo \"rustc $*\" >> \"{}\"\n",
            log_path.display()
        )
    }
}

pub(crate) fn fake_version_tool_script(log_path: &Path, tool_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo {0} cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME% args=%*>>\"{1}\"\n\
             echo {0} 1.0.0 (fake)\n",
            tool_name,
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"{0} cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}} args=$*\" >> \"{1}\"\n\
             echo \"{0} 1.0.0 (fake)\"\n",
            tool_name,
            log_path.display()
        )
    }
}

pub(crate) fn fake_rustup_script(log_path: &Path, tool_dir: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{0}\"\n\
             if \"%~1\"==\"which\" (\n\
               if \"%~2\"==\"cargo\" (\n\
                 echo {1}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustc\" (\n\
                 echo {2}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustfmt\" (\n\
                 echo {3}\n\
                 exit /b 0\n\
               )\n\
             )\n\
             echo unsupported rustup invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            tool_dir.join("cargo.cmd").display(),
            tool_dir.join("rustc.cmd").display(),
            tool_dir.join("rustfmt.cmd").display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"which\" ]; then\n\
               case \"$2\" in\n\
                 cargo)\n\
                   echo \"{1}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustc)\n\
                   echo \"{2}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustfmt)\n\
                   echo \"{3}\"\n\
                   exit 0\n\
                   ;;\n\
               esac\n\
             fi\n\
             echo \"unsupported rustup invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            tool_dir.join("cargo").display(),
            tool_dir.join("rustc").display(),
            tool_dir.join("rustfmt").display()
        )
    }
}

pub(crate) fn fake_failing_rustup_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{}\"\n\
             echo rustup should not have been invoked 1>&2\n\
             exit /b 1\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{}\"\n\
             echo \"rustup should not have been invoked\" >&2\n\
             exit 1\n",
            log_path.display()
        )
    }
}

pub(crate) fn fake_zccache_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             if \"%~1\"==\"start\" goto soldr_zccache_start\n\
             if \"%~1\"==\"stop\" (\n\
               echo zccache stop cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
               if defined SOLDR_TEST_ZCCACHE_STALE_START_ONCE type nul > \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.stopped\"\n\
               if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER type nul > \"%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%\"\n\
               exit /b 0\n\
             )\n\
              if \"%~1\"==\"session-start\" (\n\
                echo zccache session-start cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE% args=%*>>\"{0}\"\n\
                if not \"%~4\"==\"\" type nul > \"%~4\"\n\
                if not \"%~6\"==\"\" type nul > \"%~6\"\n\
                echo {{\"session_id\":\"test-session\"}}\n\
                exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-end\" (\n\
               echo zccache session-end %~2 %~3 cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
               if \"%~3\"==\"--json\" (\n\
                 echo {{\"status\":\"ok\",\"session_id\":\"test-session\",\"duration_ms\":1200,\"compilations\":10,\"hits\":7,\"misses\":3,\"non_cacheable\":2,\"errors\":1,\"time_saved_ms\":900,\"unique_sources\":4,\"bytes_read\":111,\"bytes_written\":222,\"hit_rate\":0.7}}\n\
               ) else (\n\
                 echo hits: 1\n\
               )\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"rust-plan\" (\n\
               echo zccache rust-plan %~2 cache_dir=%ZCCACHE_CACHE_DIR% args=%*>>\"{0}\"\n\
               if /I \"%~2\"==\"restore\" if defined SOLDR_TEST_RUST_PLAN_STALE (\n\
                 echo {{\"operation\":\"restore\",\"restored_file_count\":0,\"artifact_absent_from_restored_plan\":1,\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\n\
                 exit /b 0\n\
               )\n\
               echo {{\"operation\":\"%~2\",\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"status\" (\n\
               if defined SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER (\n\
                 if exist \"%SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER%\" (\n\
                   echo daemon not running 1>&2\n\
                   exit /b 1\n\
                 )\n\
               )\n\
               echo hits=7\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"flush\" goto soldr_zccache_flush\n\
             if \"%~1\"==\"clear\" (\n\
               echo zccache clear cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo zccache wrapper cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE% %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n\
             :soldr_zccache_start\n\
             echo zccache start cache_dir=%ZCCACHE_CACHE_DIR% daemon_namespace=%ZCCACHE_DAEMON_NAMESPACE%>>\"{0}\"\n\
             if defined SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE goto soldr_zccache_start_stale_lock\n\
             if not defined SOLDR_TEST_ZCCACHE_STALE_START_ONCE exit /b 0\n\
             if exist \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.stopped\" exit /b 0\n\
             if not exist \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.failed\" (\n\
               type nul > \"%SOLDR_TEST_ZCCACHE_STALE_START_ONCE%.failed\"\n\
               echo failed to start daemon: daemon process 3197 exists but not accepting connections 1>&2\n\
               exit /b 1\n\
             )\n\
             echo zccache start retried before stop 1>&2\n\
             exit /b 66\n\
             :soldr_zccache_start_stale_lock\n\
             if not defined ZCCACHE_CACHE_DIR (\n\
               echo zccache start missing ZCCACHE_CACHE_DIR 1>&2\n\
               exit /b 67\n\
             )\n\
             if not exist \"%SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE%.failed\" (\n\
               if not exist \"%ZCCACHE_CACHE_DIR%\" mkdir \"%ZCCACHE_CACHE_DIR%\"\n\
               echo 3197>\"%ZCCACHE_CACHE_DIR%\\daemon.lock\"\n\
               type nul > \"%SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE%.failed\"\n\
               echo failed to start daemon: daemon started but not accepting connections after 10s 1>&2\n\
               exit /b 1\n\
             )\n\
             if exist \"%ZCCACHE_CACHE_DIR%\\daemon.lock\" (\n\
               echo zccache start retried while stale daemon.lock remained 1>&2\n\
               exit /b 66\n\
             )\n\
             exit /b 0\n\
             :soldr_zccache_flush\n\
             echo zccache flush args=%* cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
             if defined SOLDR_TEST_ZCCACHE_FLUSH_UNSUPPORTED goto soldr_zccache_flush_unsupported\n\
             if not \"%~2\"==\"--json\" goto soldr_zccache_flush_plain\n\
             if defined SOLDR_TEST_ZCCACHE_FLUSH_NO_JSON goto soldr_zccache_flush_no_json\n\
             echo {{\"status\":\"ok\",\"bytes_written\":4096,\"duration_ms\":12}}\n\
             exit /b 0\n\
             :soldr_zccache_flush_plain\n\
             echo flushed\n\
             exit /b 0\n\
             :soldr_zccache_flush_unsupported\n\
             echo error: unrecognized subcommand 'flush' 1>&2\n\
             exit /b 2\n\
             :soldr_zccache_flush_no_json\n\
             echo error: unexpected argument '--json' found 1>&2\n\
             exit /b 2\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               start)\n\
                 echo \"zccache start cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE:-}}\" ]; then\n\
                   if [ -z \"${{ZCCACHE_CACHE_DIR:-}}\" ]; then\n\
                     echo 'zccache start missing ZCCACHE_CACHE_DIR' >&2\n\
                     exit 67\n\
                   fi\n\
                   lock_path=\"${{ZCCACHE_CACHE_DIR}}/daemon.lock\"\n\
                   if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE}}.failed\" ]; then\n\
                     mkdir -p \"${{ZCCACHE_CACHE_DIR}}\"\n\
                     printf '3197\\n' > \"$lock_path\"\n\
                     : > \"${{SOLDR_TEST_ZCCACHE_STALE_LOCK_ONCE}}.failed\"\n\
                     echo 'failed to start daemon: daemon started but not accepting connections after 10s' >&2\n\
                     exit 1\n\
                   fi\n\
                   if [ -e \"$lock_path\" ]; then\n\
                     echo 'zccache start retried while stale daemon.lock remained' >&2\n\
                     exit 66\n\
                   fi\n\
                 fi\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE:-}}\" ]; then\n\
                   if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.stopped\" ]; then\n\
                     if [ ! -e \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.failed\" ]; then\n\
                       : > \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.failed\"\n\
                       echo 'failed to start daemon: daemon process 3197 exists but not accepting connections' >&2\n\
                       exit 1\n\
                     fi\n\
                     echo 'zccache start retried before stop' >&2\n\
                     exit 66\n\
                   fi\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
               stop)\n\
                 echo \"zccache stop cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE:-}}\" ]; then\n\
                   : > \"${{SOLDR_TEST_ZCCACHE_STALE_START_ONCE}}.stopped\"\n\
                 fi\n\
                 if [ -n \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}}\" ]; then\n\
                   : > \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER}}\"\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
                session-start)\n\
                  echo \"zccache session-start cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}} args=$*\" >> \"{0}\"\n\
                  : > \"$4\"\n\
                  : > \"$6\"\n\
                  echo '{{\"session_id\":\"test-session\"}}'\n\
                  exit 0\n\
               ;;\n\
               session-end)\n\
                 echo \"zccache session-end $2 $3 cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{0}\"\n\
                 if [ \"${{3:-}}\" = \"--json\" ]; then\n\
                   printf '{{\"status\":\"ok\",\"session_id\":\"test-session\",\"duration_ms\":1200,\"compilations\":10,\"hits\":7,\"misses\":3,\"non_cacheable\":2,\"errors\":1,\"time_saved_ms\":900,\"unique_sources\":4,\"bytes_read\":111,\"bytes_written\":222,\"hit_rate\":0.7}}\\n'\n\
                 else\n\
                   echo 'hits: 1'\n\
                 fi\n\
                 exit 0\n\
                 ;;\n\
              rust-plan)\n\
                echo \"zccache rust-plan $2 cache_dir=${{ZCCACHE_CACHE_DIR:-}} args=$*\" >> \"{0}\"\n\
                if [ \"$2\" = \"restore\" ] && [ -n \"${{SOLDR_TEST_RUST_PLAN_STALE:-}}\" ]; then\n\
                  printf '{{\"operation\":\"restore\",\"restored_file_count\":0,\"artifact_absent_from_restored_plan\":1,\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\\n'\n\
                  exit 0\n\
                fi\n\
                printf '{{\"operation\":\"%s\",\"compatibility\":{{\"status\":\"ok\",\"errors\":[]}}}}\\n' \"$2\"\n\
                exit 0\n\
                ;;\n\
              status)\n\
                if [ -n \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER:-}}\" ] && [ -e \"${{SOLDR_TEST_ZCCACHE_DAEMON_DOWN_MARKER}}\" ]; then\n\
                  echo 'daemon not running' >&2\n\
                  exit 1\n\
                fi\n\
                echo 'hits=7'\n\
                exit 0\n\
                ;;\n\
              flush)\n\
                echo \"zccache flush args=$* cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                if [ -n \"${{SOLDR_TEST_ZCCACHE_FLUSH_UNSUPPORTED:-}}\" ]; then\n\
                  echo \"error: unrecognized subcommand 'flush'\" >&2\n\
                  exit 2\n\
                fi\n\
                if [ \"${{2:-}}\" = \"--json\" ]; then\n\
                  if [ -n \"${{SOLDR_TEST_ZCCACHE_FLUSH_NO_JSON:-}}\" ]; then\n\
                    echo \"error: unexpected argument '--json' found\" >&2\n\
                    exit 2\n\
                  fi\n\
                  printf '{{\"status\":\"ok\",\"bytes_written\":4096,\"duration_ms\":12}}\\n'\n\
                else\n\
                  echo 'flushed'\n\
                fi\n\
                exit 0\n\
                ;;\n\
               clear)\n\
                 echo \"zccache clear cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
             esac\n\
             rustc=\"$1\"\n\
             shift\n\
             if [ -n \"${{SOLDR_TEST_JOBSERVER_READ_FD:-}}\" ]; then\n\
               if ! eval \": <&$SOLDR_TEST_JOBSERVER_READ_FD\"; then\n\
                 echo \"jobserver read fd $SOLDR_TEST_JOBSERVER_READ_FD is not open\" >&2\n\
                 exit 42\n\
               fi\n\
               if ! eval \": >&$SOLDR_TEST_JOBSERVER_WRITE_FD\"; then\n\
                 echo \"jobserver write fd $SOLDR_TEST_JOBSERVER_WRITE_FD is not open\" >&2\n\
                 exit 42\n\
               fi\n\
               echo \"zccache jobserver fds ok read=$SOLDR_TEST_JOBSERVER_READ_FD write=$SOLDR_TEST_JOBSERVER_WRITE_FD\" >> \"{0}\"\n\
             fi\n\
             echo \"zccache wrapper cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}} $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display()
        )
    }
}

pub(crate) fn fake_custom_wrapper_script(log_path: &Path, wrapper_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo {1} wrapper %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            wrapper_name
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             rustc=\"$1\"\n\
             shift\n\
             echo \"{1} wrapper $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display(),
            wrapper_name
        )
    }
}

pub(crate) fn install_fake_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");
    write_fake_script(&cargo, &fake_cargo_script(log_path));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (cargo, rustc, zccache)
}

#[cfg(not(windows))]
pub(crate) fn install_fake_jobserver_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-jobserver-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");

    write_fake_script(&cargo, &fake_cargo_with_jobserver_script(log_path));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (cargo, rustc, zccache)
}

pub(crate) fn install_fake_version_toolchain(
    tool_dir: &Path,
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf) {
    let cargo = fake_script_path(tool_dir, "cargo");
    let rustc = fake_script_path(tool_dir, "rustc");
    let rustfmt = fake_script_path(tool_dir, "rustfmt");
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    (cargo, rustc, rustfmt)
}

pub(crate) fn install_fake_wrapper(log_path: &Path, wrapper_name: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-wrapper");
    let wrapper = fake_script_path(&dir, wrapper_name);
    write_fake_script(
        &wrapper,
        &fake_custom_wrapper_script(log_path, wrapper_name),
    );
    wrapper
}

pub(crate) fn install_fake_rustup_toolchain(
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-rustup-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let rustfmt = fake_script_path(&dir, "rustfmt");
    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    write_fake_script(&rustup, &fake_rustup_script(log_path, &dir));
    (rustup, cargo, rustc, rustfmt)
}

pub(crate) fn install_failing_fake_rustup(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-failure");
    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");
    write_fake_script(&rustup, &fake_failing_rustup_script(log_path));
    rustup
}

/// Fake rustup that logs one line per invocation to `log_path`, with the
/// argv joined by `\u{1f}` (ASCII unit separator) so test assertions can
/// reason about the exact argv even when individual arguments contain
/// spaces. Always exits 0.
pub(crate) fn fake_logging_rustup_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{}\"\n\
             exit /b 0\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        // Use ASCII Unit Separator (\037) between argv elements so assertions
        // can split deterministically even if an individual arg contains
        // spaces. Hand-roll the join in /bin/sh.
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

/// Install a fake rustup that logs argv per invocation. Returns the path
/// to the fake binary, ready to hand to `SOLDR_TEST_RUSTUP_BIN`.
pub(crate) fn install_logging_fake_rustup(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-logging");
    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");
    write_fake_script(&rustup, &fake_logging_rustup_script(log_path));
    rustup
}

/// Read every argv invocation logged by `fake_logging_rustup_script`.
/// Each returned `Vec<String>` is one invocation, with argv split on the
/// ASCII unit separator.
pub(crate) fn read_logged_rustup_invocations(log_path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(log_path).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect()
}

pub(crate) fn seed_rust_toolchain_toml(dir: &Path, contents: &str) {
    fs::create_dir_all(dir).expect("failed to create workspace dir");
    fs::write(dir.join("rust-toolchain.toml"), contents)
        .expect("failed to write rust-toolchain.toml");
}

/// Fake cargo that logs one line per invocation to `log_path`, with the
/// argv joined by `\u{1f}` (ASCII unit separator) so test assertions can
/// reason about the exact argv even when individual arguments contain
/// spaces. Always exits 0. Mirrors `fake_logging_rustup_script` but
/// writes to a separate log file so concurrent rustup + cargo
/// invocations under `toolchain prepare` don't interleave.
pub(crate) fn fake_logging_cargo_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{}\"\n\
             exit /b 0\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        )
    }
}

/// Install a fake cargo that logs argv per invocation. Returns the path
/// to the fake binary, ready to hand to `SOLDR_TEST_CARGO_BIN`.
pub(crate) fn install_logging_fake_cargo(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-cargo-logging");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(&cargo, &fake_logging_cargo_script(log_path));
    cargo
}

/// Fake `cargo` that responds to `--version` with a deterministic string
/// (matching the real cargo `cargo X.Y.Z (sha date)` format used by
/// `toolchain ensure --json`'s smoke verify) AND otherwise logs argv to
/// `log_path`. Useful for the `ensure` test which needs both a valid
/// `--version` capture AND to assert `cargo install` argv for plugins.
///
/// `version` must not contain literal `(` / `)` on Windows because cmd.exe
/// `echo` treats those as block delimiters — we substitute them through the
/// caret-escape `^(` / `^)` form on Windows automatically.
pub(crate) fn fake_logging_versioned_cargo_script(log_path: &Path, version: &str) -> String {
    #[cfg(windows)]
    {
        let escaped = escape_for_cmd_echo(version);
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo {1}\n\
               exit /b 0\n\
             )\n\
             setlocal enabledelayedexpansion\n\
             set \"line=\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if defined line (set \"line=!line!\u{1f}%~1\") else (set \"line=%~1\")\n\
             shift\n\
             goto loop\n\
             :done\n\
             echo !line!>>\"{0}\"\n\
             exit /b 0\n",
            log_path.display(),
            escaped
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo '{1}'\n\
               exit 0\n\
             fi\n\
             sep=$(printf '\\037')\n\
             out=\"\"\n\
             first=1\n\
             for arg in \"$@\"; do\n\
               if [ $first -eq 1 ]; then\n\
                 out=\"$arg\"\n\
                 first=0\n\
               else\n\
                 out=\"$out${{sep}}$arg\"\n\
               fi\n\
             done\n\
             printf '%s\\n' \"$out\" >> \"{0}\"\n\
             exit 0\n",
            log_path.display(),
            version
        )
    }
}

/// Escape `(` and `)` for inclusion in a cmd.exe `echo` line inside an
/// `if (...) ( ... )` block (where the un-escaped parens close the block
/// prematurely). The standard escape is `^(` / `^)`.
#[cfg(windows)]
fn escape_for_cmd_echo(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '(' => out.push_str("^("),
            ')' => out.push_str("^)"),
            other => out.push(other),
        }
    }
    out
}

/// Install a fake `cargo` that responds to `--version` AND logs argv on
/// every other invocation. Returns the path to the fake binary.
pub(crate) fn install_logging_versioned_fake_cargo(log_path: &Path, version: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-cargo-versioned");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(
        &cargo,
        &fake_logging_versioned_cargo_script(log_path, version),
    );
    cargo
}

/// Fake `rustc` that responds to `--version` (deterministic) and
/// otherwise exits 0. Used by the ensure JSON smoke-verify test.
pub(crate) fn fake_versioned_rustc_script(version: &str) -> String {
    #[cfg(windows)]
    {
        let escaped = escape_for_cmd_echo(version);
        format!(
            "@echo off\n\
             if \"%~1\"==\"--version\" (\n\
               echo {0}\n\
               exit /b 0\n\
             )\n\
             exit /b 0\n",
            escaped
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then\n\
               echo '{0}'\n\
               exit 0\n\
             fi\n\
             exit 0\n",
            version
        )
    }
}

/// Install a fake `rustc` that responds to `--version` deterministically.
/// Suitable for `SOLDR_TEST_RUSTC_BIN`.
pub(crate) fn install_versioned_fake_rustc(version: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-rustc-versioned");
    let rustc = fake_script_path(&dir, "rustc");
    write_fake_script(&rustc, &fake_versioned_rustc_script(version));
    rustc
}

/// Fake `rustc` that always exits non-zero on `--version`. Used to
/// exercise the `smoke_verify.ok == false` branch.
pub(crate) fn fake_failing_rustc_script() -> String {
    #[cfg(windows)]
    {
        "@echo off\n\
         echo simulated rustc failure 1>&2\n\
         exit /b 1\n"
            .to_string()
    }
    #[cfg(not(windows))]
    {
        "#!/bin/sh\n\
         echo 'simulated rustc failure' >&2\n\
         exit 1\n"
            .to_string()
    }
}

/// Install a fake `rustc` that fails any `--version` call.
pub(crate) fn install_failing_fake_rustc() -> PathBuf {
    let dir = unique_temp_dir("fake-rustc-failing");
    let rustc = fake_script_path(&dir, "rustc");
    write_fake_script(&rustc, &fake_failing_rustc_script());
    rustc
}

/// Read every argv invocation logged by `fake_logging_cargo_script`.
/// Each returned `Vec<String>` is one invocation, with argv split on
/// the ASCII unit separator.
pub(crate) fn read_logged_cargo_invocations(log_path: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(log_path).unwrap_or_default();
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split('\u{1f}').map(str::to_string).collect())
        .collect()
}

pub(crate) fn prepend_to_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("failed to join PATH")
}

/// PATH value for tests that need to verify soldr's tool resolution falls back
/// to its rustup path. Strips the runner's real cargo/rustc entries so
/// `probe_toolchain_binary`'s PATH search can't shadow the in-test fakes.
/// On Windows we keep `System32` so `Command::new` can still spawn `.cmd`
/// shims via `cmd.exe`.
pub(crate) fn isolated_test_path() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let dirs = [system_root.join("System32"), system_root];
        std::env::join_paths(dirs).expect("failed to join isolated PATH")
    }
    #[cfg(not(windows))]
    {
        std::ffi::OsString::from("/usr/bin:/bin")
    }
}
