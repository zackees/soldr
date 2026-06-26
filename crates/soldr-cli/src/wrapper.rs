//! `RUSTC_WRAPPER` invocation path: forwards rustc / clippy-driver through
//! the daemon's embedded zccache compile service via the `Request::Compile`
//! IPC verb, spills stdin to a temp file when cargo passes `-`. Extracted
//! from `main.rs` as part of issue #339. The legacy fork-zccache.exe
//! wrapper path was removed in #980 L1 second pass — embedded is mandatory.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::startup_profile::WrapperProfile;
use crate::{apply_implicit_toolchain_homes, resolve_toolchain_binary};

/// Known toolchain binaries that cargo may invoke through RUSTC_WRAPPER
/// or RUSTC_WORKSPACE_WRAPPER. When soldr is set as a wrapper, cargo
/// passes: `soldr <toolchain-binary> <rustc-args...>`
const WRAPPER_PASSTHROUGH_TOOLS: &[&str] = &["rustc", "clippy-driver"];

pub(crate) fn is_wrapper_invocation(arg: &str) -> bool {
    let stem = std::path::Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);

    WRAPPER_PASSTHROUGH_TOOLS.contains(&stem)
}

/// Detect rustc invocations cargo issues through `RUSTC_WRAPPER` that
/// can never benefit from zccache (issue #980, L2):
///
///   * Any `--print`/`--print=...` probe — these don't compile, they
///     just print metadata (e.g. `--print sysroot`, `--print cfg`).
///   * `--emit` invocations whose emit-kind list is exclusively
///     `dep-info` — cargo's dep-graph refresh pass produces no object
///     code, so caching it via the wrapper is pure overhead.
///
/// `args` is the full argv as received in wrapper mode: `args[0]` is
/// the soldr binary, `args[1]` is the tool path (rustc / clippy-driver),
/// and `args[2..]` are the rustc arguments. The caller is responsible
/// for confirming the tool stem is `rustc` before consulting this
/// predicate — clippy-driver does its own routing.
pub(crate) fn is_non_cacheable_rustc(args: &[String]) -> bool {
    if args.len() < 3 {
        return false;
    }
    let rest = &args[2..];

    // `--print <mode>` or `--print=<mode>` never compiles.
    if rest
        .iter()
        .any(|a| a == "--print" || a.starts_with("--print="))
    {
        return true;
    }

    // Version probes never compile. Cargo's very first call when it
    // resolves a toolchain is `rustc -vV`; the wrapper must direct-exec
    // it instead of routing through the daemon (which doesn't help and
    // costs the daemon-spawn latency on cold startups).
    if rest
        .iter()
        .any(|a| a == "-vV" || a == "-V" || a == "--version")
    {
        return true;
    }

    // Collect every emit-kind across all `--emit`/`--emit=` occurrences.
    // Cargo can spell either form (and may pass multiple), so accumulate
    // the union before deciding.
    let mut has_emit = false;
    let mut emit_kinds: Vec<&str> = Vec::new();
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        if a == "--emit" {
            if let Some(v) = iter.next() {
                has_emit = true;
                emit_kinds.extend(v.split(','));
            }
        } else if let Some(v) = a.strip_prefix("--emit=") {
            has_emit = true;
            emit_kinds.extend(v.split(','));
        }
    }
    if has_emit && !emit_kinds.is_empty() {
        // `--emit=dep-info=foo.d` is still a dep-info-only emit; strip
        // the optional `=path` suffix before classifying.
        let only_dep_info = emit_kinds
            .iter()
            .all(|k| k.trim().split('=').next().unwrap_or("") == "dep-info");
        if only_dep_info {
            return true;
        }
    }

    false
}

pub(crate) fn run_rustc_wrapper(
    raw_args: &[String],
    mut profile: WrapperProfile,
) -> Result<i32, SoldrError> {
    let tool_arg = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing tool path in wrapper mode".into()))?;

    let tool_stem = std::path::Path::new(tool_arg.as_str())
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(tool_arg);

    profile.mark("tool_resolved");

    // Per-build target/ tracking for `soldr gc`. Best-effort: if we
    // can't resolve a workspace target dir cheaply, or the redb
    // upsert fails for any reason, skip silently — never fail a build.
    //
    // The phase emitted afterwards tells SOLDR_PROFILE_STARTUP=1
    // consumers which routing path fired — explicitly distinguishing
    // the daemon path from the fast direct-redb path proves the
    // Option-A invariant from #474: outside a soldr-cargo session,
    // no `daemon`/`is_live`/`socket`/`record_target_touch_or_fallback`
    // phase appears in the profile.
    if tool_stem == "rustc" {
        let path = record_target_dir_in_registry(&raw_args[2..]);
        let mark = match path {
            TargetTouchPath::NoTarget => "target_dir_recorded_no_target",
            TargetTouchPath::NoPaths => "target_dir_recorded_no_paths",
            TargetTouchPath::FastDirect => "target_dir_recorded_fast",
            TargetTouchPath::DaemonFirst => "target_dir_recorded_daemon",
            TargetTouchPath::MemoSkipped => "target_dir_recorded_memo",
        };
        profile.mark(mark);
    } else {
        profile.mark("target_dir_recorded");
    }

    // When the source argument is "-" (stdin), rustc reads the source from
    // the process's stdin. If we pass this invocation to zccache as-is,
    // zccache reads stdin to hash the source content, exhausting the pipe
    // before rustc is spawned. Rustc then receives an empty stdin, compiles
    // nothing, and exits 0 — masking any real compile error (e.g. E0554 from
    // build-script feature probes like rustix 0.37's `can_compile()`).
    //
    // Fix: spill stdin to a content-addressed temp file so both zccache and
    // rustc see a stable real path. This keeps zccache in the loop (it can
    // hash the file normally) while preserving the correct exit code, and it
    // lets identical feature probes converge on the same cache key.
    let stdin_tempfile = if raw_args[2..].iter().any(|a| a == "-") {
        Some(spill_stdin_to_content_addressed_file()?)
    } else {
        None
    };
    profile.mark("stdin_handled");

    // Build the effective arg list, replacing "-" with the temp file path.
    let effective_args: std::borrow::Cow<[String]> = if let Some(ref tmp) = stdin_tempfile {
        let tmp_str = tmp.path().to_string_lossy().into_owned();
        let replaced: Vec<String> = raw_args
            .iter()
            .cloned()
            .map(|a| if a == "-" { tmp_str.clone() } else { a })
            .collect();
        std::borrow::Cow::Owned(replaced)
    } else {
        std::borrow::Cow::Borrowed(raw_args)
    };

    // L2 cold-build skip (issue #980): some rustc invocations cargo
    // issues through RUSTC_WRAPPER can NEVER hit the cache — `--print
    // sysroot` / `--print cfg` probes do not compile anything, and
    // `--emit=dep-info`-only runs only refresh the dep graph. Routing
    // those through zccache pays the full wrapper startup tax (process
    // spawn, daemon IPC) for guaranteed-zero benefit. Detect them here
    // and let the existing direct-exec tool-spawn path below handle
    // them instead of the zccache routing block.
    let non_cacheable = tool_stem == "rustc" && is_non_cacheable_rustc(&effective_args);
    if non_cacheable {
        tracing::debug!("soldr: rustc invocation is non-cacheable; bypassing zccache");
        profile.mark("non_cacheable_bypass");
    }

    // Only route through the daemon's embedded compile service for
    // actual rustc invocations, not clippy-driver or other workspace
    // wrappers.
    if tool_stem == "rustc"
        && !non_cacheable
        && crate::cache_lib::cache_enabled_in_current_process()
    {
        // L1 (issue #977 / #980 L1): dispatch the rustc invocation to
        // the daemon's embedded zccache service over IPC. As of the
        // L1 second pass there is no fallback — embedded is
        // mandatory. The legacy `zccache.exe` fork was deleted.
        // `compile_via_daemon` either returns the daemon's reply or
        // an error that the wrapper propagates to cargo.
        profile.finish("before_embedded_compile_ipc");
        return compile_via_daemon(&effective_args);
    }

    // Resolve the tool binary. If it's already a full path, use it
    // directly. Otherwise resolve via rustup.
    let tool_path: std::path::PathBuf = if std::path::Path::new(tool_arg.as_str()).is_absolute() {
        tool_arg.into()
    } else {
        resolve_toolchain_binary(tool_stem)?
    };

    let mut command = std::process::Command::new(tool_path);
    command.args(&effective_args[2..]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    profile.finish("before_tool_spawn");
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

struct StdinSourceFile {
    path: std::path::PathBuf,
}

impl StdinSourceFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// Read all of stdin into a content-addressed source file and return it.
///
/// The file has a `.rs` extension so rustc accepts it without flags, and
/// lives in the system temp directory as `soldr-stdin-<short_blake3>.rs`.
/// It is intentionally retained so concurrent identical probes can share the
/// same stable path.
fn spill_stdin_to_content_addressed_file() -> Result<StdinSourceFile, SoldrError> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SoldrError::Other(format!("failed to read stdin: {e}")))?;
    materialize_stdin_source(&buf)
}

fn materialize_stdin_source(bytes: &[u8]) -> Result<StdinSourceFile, SoldrError> {
    let hash = blake3::hash(bytes);
    let hex = hash.to_hex();
    let temp_dir = std::env::temp_dir();
    let short_path = temp_dir.join(format!("soldr-stdin-{}.rs", &hex[..16]));
    if ensure_stdin_source_path(&short_path, bytes)? {
        return Ok(StdinSourceFile { path: short_path });
    }

    let full_path = temp_dir.join(format!("soldr-stdin-{hex}.rs"));
    if ensure_stdin_source_path(&full_path, bytes)? {
        return Ok(StdinSourceFile { path: full_path });
    }

    Err(SoldrError::Other(format!(
        "stdin temp path hash collision at {}",
        full_path.display()
    )))
}

fn ensure_stdin_source_path(path: &std::path::Path, bytes: &[u8]) -> Result<bool, SoldrError> {
    use std::io::Write as _;

    match std::fs::read(path) {
        Ok(existing) => return Ok(existing == bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(SoldrError::Other(format!(
                "failed to read existing stdin temp file {}: {err}",
                path.display()
            )));
        }
    }

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
        SoldrError::Other(format!(
            "failed to create stdin temp file in {}: {e}",
            parent.display()
        ))
    })?;
    tmp.write_all(bytes)
        .map_err(|e| SoldrError::Other(format!("failed to write stdin temp file: {e}")))?;
    let _ = tmp.as_file().sync_all();

    match tmp.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = err.file.close();
            let existing = std::fs::read(path).map_err(|e| {
                SoldrError::Other(format!(
                    "failed to read raced stdin temp file {}: {e}",
                    path.display()
                ))
            })?;
            Ok(existing == bytes)
        }
        Err(err) => Err(SoldrError::Other(format!(
            "failed to publish stdin temp file {}: {}",
            path.display(),
            err.error
        ))),
    }
}

// Routing logic + `TargetTouchPath` live in `wrapper_target.rs` so the
// integration tests in `tests/cli_wrapper_perf.rs` can drive
// `record_target_dir_in_registry` in-process via the lib's
// `pub mod wrapper_target;` declaration. Re-exported here so existing
// bin-side call sites in this file keep working unchanged.
pub(crate) use crate::wrapper_target::{record_target_dir_in_registry, TargetTouchPath};

// =========================================================================
// L1 daemon-embedded compile path (issue #977 / #980 L1)
// =========================================================================

/// Predicate for [`compile_via_daemon`]'s env-var filter (Phase 5c
/// from #981). Returns `true` for env vars that rustc, cargo build
/// scripts (cc-rs / bindgen / proc-macro setup), or zccache's internal
/// machinery actually read. Everything else is dropped from the
/// `Request::Compile` payload to keep the daemon's tokio runtime out
/// of prost serialization of ~10-50 KB of useless `CARGO_PKG_*`
/// metadata per compile.
fn is_compile_env_var(name: &str) -> bool {
    // Prefix matches first — order roughly by hit rate so the common
    // case short-circuits early on the per-call hot path.
    const PREFIXES: &[&str] = &[
        "CARGO_",   // CARGO_PKG_*, CARGO_CFG_*, CARGO_MANIFEST_DIR, etc.
        "RUSTC_",   // RUSTC, RUSTC_WRAPPER, RUSTC_BOOTSTRAP, ...
        "RUST",     // RUSTFLAGS, RUSTDOC*, RUSTFMT, RUSTUP*, RUST_BACKTRACE
        "SOLDR_",   // SOLDR_ZCCACHE_BIN, SOLDR_CACHE_*, SOLDR_BUILD_SESSION_*
        "ZCCACHE_", // ZCCACHE_CACHE_DIR, ZCCACHE_PATH_REMAP, ZCCACHE_SESSION_ID
        "CC_",      // cc-rs cc_PROFILE_TARGET style
        "CXX_",     // ditto for C++
        "AR_",
        "LD_",      // LD_LIBRARY_PATH, LD_PRELOAD — both meaningful to subprocesses
        "DEP_",     // build-script-emitted DEP_<pkg>_<key> env vars
        "OUT_",     // OUT_DIR (build script working dir)
    ];
    if PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    // Exact matches for shorter vars rustc / cc-rs / linker need.
    matches!(
        name,
        "HOME"
            | "USER"
            | "PATH"
            | "LANG"
            | "LC_ALL"
            | "TARGET"
            | "HOST"
            | "TMPDIR"
            | "TMP"
            | "TEMP"
            | "USERPROFILE"  // Windows home
            | "APPDATA"
            | "LOCALAPPDATA"
            | "PROGRAMFILES"
            | "PROGRAMDATA"
            | "WINDIR"
            | "SYSTEMROOT"
            | "SYSTEMDRIVE"
            | "COMSPEC"
            | "PATHEXT"
            | "TERM"
            | "RUSTUP_HOME"
            | "RUSTUP_TOOLCHAIN"
            | "CARGO"
            | "CARGO_HOME"
            | "CC"
            | "CXX"
            | "AR"
            | "LD"
            | "NM"
            | "OBJCOPY"
            | "OBJDUMP"
            | "STRIP"
            | "RANLIB"
            | "CFLAGS"
            | "CXXFLAGS"
            | "LDFLAGS"
            | "MSYSTEM"  // MinGW shell detection (cc-rs reads this)
            | "VCINSTALLDIR"  // MSVC build-script env vars
            | "VSINSTALLDIR"
            | "VCToolsInstallDir"
            | "WindowsSdkDir"
            | "INCLUDE"
            | "LIB"
            | "LIBPATH"
    )
}

/// Dispatch the rustc compile to the running daemon's embedded zccache
/// service over the `Request::Compile` IPC verb. Returns the exit code
/// to propagate to cargo, or a hard error if the daemon is unreachable
/// or replies with anything other than a `CompileResponse`. The legacy
/// fork-zccache.exe fallback was removed in #980 L1 second pass —
/// embedded is mandatory.
fn compile_via_daemon(effective_args: &[String]) -> Result<i32, SoldrError> {
    use crate::daemon::client;

    let paths =
        SoldrPaths::new().map_err(|e| SoldrError::Other(format!("resolve soldr paths: {e}")))?;
    let sock = client::default_sock_path(&paths);

    // Build the Compile request. The wrapper's argv layout is
    // [soldr, rustc-path, rustc-args...]; we strip the soldr binary
    // (argv[0]) and send rustc-path as args[0], rustc-args as args[1..].
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    // Phase 5c (#981): forward only the env vars rustc + cc-rs build
    // scripts actually use. The full `std::env::vars()` payload in a
    // deep cargo build is 10-50 KB per compile (mostly cargo-injected
    // CARGO_PKG_* metadata that's already in the args), and pushing
    // it through prost on every Request::Compile dominates the
    // daemon's tokio-rt-worker cost on cold cache.
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| is_compile_env_var(k))
        .collect();
    let req = crate::daemon::protocol::CompileRequest {
        args: effective_args.iter().skip(1).cloned().collect(),
        cwd,
        env,
        stdin: Vec::new(),
    };

    // First try — daemon may already be running. #983 Phase 5b: the
    // wrapper streams rustc stdout/stderr directly to its own
    // stdout/stderr as chunk frames arrive, so the IPC layer never
    // holds the whole rustc output buffered in memory.
    let first = client::compile_streaming(&sock, req.clone(), std::io::stdout(), std::io::stderr());
    let done = match first {
        Ok(info) => info,
        Err(_) => {
            // Spawn the daemon, then retry up to ~30 s. The spawn
            // returns before the socket is bound. On cold WSL2 starts
            // (or fresh container layers) the daemon's embedded
            // zccache service can take a couple of seconds to come up
            // before it accepts IPC.
            let spawn_result = crate::daemon::lifecycle::try_spawn_detached();
            if let Err(e) = &spawn_result {
                eprintln!("soldr: try_spawn_detached returned err: {e:?}");
            }
            let mut last_err = None;
            let mut done = None;
            // 30 s retry window — embedded zccache cold-start (redb
            // open, cache root init, depgraph load) can take several
            // seconds on first-ever boot in a container.
            for attempt in 0..300 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                match client::compile_streaming(
                    &sock,
                    req.clone(),
                    std::io::stdout(),
                    std::io::stderr(),
                ) {
                    Ok(info) => {
                        done = Some(info);
                        break;
                    }
                    Err(e) => last_err = Some((attempt, e)),
                }
            }
            match done {
                Some(info) => info,
                None => {
                    // Diagnose: report whether the daemon binary even
                    // exists at the expected sibling path, the PID
                    // file presence, and the tail of the daemon's
                    // spawn log (the daemon's stderr redirected by
                    // spawn_detached_inner). Concrete place to look.
                    let bin_diag = std::env::current_exe()
                        .ok()
                        .map(|p| crate::daemon::service_definition::sibling_daemon_binary(&p))
                        .map(|p| (p.exists(), p.display().to_string()))
                        .unwrap_or((false, "<unknown>".into()));
                    let log_tail = SoldrPaths::new()
                        .ok()
                        .map(|p| p.root.join("daemon-spawn.log"))
                        .and_then(|p| std::fs::read_to_string(&p).ok())
                        .map(|s| {
                            s.lines()
                                .rev()
                                .take(20)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_else(|| "<no spawn log>".into());
                    return Err(SoldrError::Other(format!(
                        "soldr daemon embedded compile dispatch failed after spawn + 30s retry: \
                         {:?}. daemon_binary=({}, exists={}) spawn_result={:?} sock={}.\n\
                         daemon-spawn.log tail:\n{}\n\
                         The legacy fork-zccache.exe fallback was removed in #980 L1; \
                         confirm the soldr-daemon binary is present alongside the soldr binary.",
                        last_err, bin_diag.1, bin_diag.0,
                        spawn_result,
                        sock.display(),
                        log_tail,
                    )));
                }
            }
        }
    };

    Ok(done.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_source_path_is_content_addressed() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let bytes = format!("fn main() {{ let _ = {nonce}; }}\n");
        let file = materialize_stdin_source(bytes.as_bytes()).unwrap();
        let hash = blake3::hash(bytes.as_bytes()).to_hex();
        let name = file.path().file_name().unwrap().to_string_lossy();

        assert_eq!(name.as_ref(), format!("soldr-stdin-{}.rs", &hash[..16]));
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes.as_bytes());

        let same = materialize_stdin_source(bytes.as_bytes()).unwrap();
        assert_eq!(same.path(), file.path());
    }

    #[test]
    fn stdin_source_paths_do_not_collide_for_distinct_content() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let a = materialize_stdin_source(format!("const A: u128 = {nonce};\n").as_bytes()).unwrap();
        let b = materialize_stdin_source(format!("const B: u128 = {};\n", nonce + 1).as_bytes())
            .unwrap();

        assert_ne!(a.path(), b.path());
    }

    // -------- is_non_cacheable_rustc (issue #980, L2) --------

    /// Build a wrapper-shaped argv: argv[0] is the soldr binary, argv[1]
    /// is the tool path, argv[2..] are the rustc args under test.
    fn wrapper_argv(rustc_args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec!["soldr".into(), "rustc".into()];
        v.extend(rustc_args.iter().map(|s| (*s).to_string()));
        v
    }

    crate::timed_test!(non_cacheable_print_mode_detected, {
        // `--print=cfg` is a metadata probe — never compiles, never
        // benefits from zccache.
        let argv = wrapper_argv(&["--print=cfg"]);
        assert!(is_non_cacheable_rustc(&argv));
    });

    crate::timed_test!(non_cacheable_emit_dep_info_only_detected, {
        // Cargo's dep-graph refresh: `--emit=dep-info` produces no
        // object code, so wrapper round-trip is wasted.
        let argv = wrapper_argv(&["--emit=dep-info", "--crate-name=foo"]);
        assert!(is_non_cacheable_rustc(&argv));
    });

    crate::timed_test!(cacheable_emit_link_metadata_not_flagged, {
        // A normal compile that emits link + metadata MUST still flow
        // through zccache.
        let argv = wrapper_argv(&["--emit=link,metadata"]);
        assert!(!is_non_cacheable_rustc(&argv));
    });

    crate::timed_test!(cacheable_no_emit_no_print_not_flagged, {
        // No `--emit`, no `--print` → normal compile, must remain
        // cacheable.
        let argv = wrapper_argv(&["src/lib.rs"]);
        assert!(!is_non_cacheable_rustc(&argv));
    });
}
