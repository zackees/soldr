//! `RUSTC_WRAPPER` invocation path: forwards rustc / clippy-driver through
//! the daemon's embedded zccache compile service via the `Request::Compile`
//! IPC verb, spills stdin to a temp file when cargo passes `-`. Extracted
//! from `main.rs` as part of issue #339. The legacy fork-zccache.exe
//! wrapper path was removed in #980 L1 second pass — embedded is mandatory.

use crate::core::{suppress_windows_console_window, SoldrError};
use crate::resolve_toolchain_binary;
use crate::startup_profile::WrapperProfile;

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

/// Detect rustc-style compiler invocations cargo issues through
/// `RUSTC_WRAPPER` that can never benefit from zccache (issue #980, L2):
///
///   * Any `--print`/`--print=...` probe — these don't compile, they
///     just print metadata (e.g. `--print sysroot`, `--print cfg`).
///   * `--emit` invocations whose emit-kind list is exclusively
///     `dep-info` — cargo's dep-graph refresh pass produces no object
///     code, so caching it via the wrapper is pure overhead.
///
/// `args` is the full argv as received in wrapper mode: `args[0]` is
/// the soldr binary, `args[1]` is the tool path (rustc / clippy-driver),
/// and `args[2..]` are the rustc-style arguments. With nested
/// `RUSTC_WRAPPER` + `RUSTC_WORKSPACE_WRAPPER=clippy-driver`,
/// `args[2]` is the real rustc path and `args[3..]` are the crate
/// arguments; scanning the whole tail still detects the no-op probes.
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

fn routes_through_embedded_zccache(tool_stem: &str) -> bool {
    matches!(tool_stem, "rustc" | "clippy-driver")
}

/// Cargo nests the workspace compiler inside the outer wrapper as
/// `<outer> <workspace-compiler> <real-compiler> <compile-args...>`.
/// Once soldr has selected the workspace compiler, that wrapper-only
/// compiler identity must be consumed rather than forwarded as a source
/// input. Restrict the rewrite to known executable basenames so direct
/// invocations and ordinary source paths remain byte-for-byte intact.
fn normalize_nested_workspace_wrapper_args<'a>(
    args: &'a [String],
    tool_stem: &str,
) -> std::borrow::Cow<'a, [String]> {
    if tool_stem != WRAPPER_PASSTHROUGH_TOOLS[1] || args.len() < 3 {
        return std::borrow::Cow::Borrowed(args);
    }

    let nested_tool_name = std::path::Path::new(&args[2])
        .file_name()
        .and_then(std::ffi::OsStr::to_str);
    let expected_name = WRAPPER_PASSTHROUGH_TOOLS[0];
    let is_real_compiler = nested_tool_name.is_some_and(|name| {
        name.eq_ignore_ascii_case(expected_name)
            || name.rsplit_once('.').is_some_and(|(stem, extension)| {
                stem.eq_ignore_ascii_case(expected_name)
                    && ["exe", "cmd", "bat"]
                        .iter()
                        .any(|known| extension.eq_ignore_ascii_case(known))
            })
    });
    if !is_real_compiler {
        return std::borrow::Cow::Borrowed(args);
    }

    let mut normalized = Vec::with_capacity(args.len() - 1);
    normalized.extend_from_slice(&args[..2]);
    normalized.extend_from_slice(&args[3..]);
    std::borrow::Cow::Owned(normalized)
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
    let compile_args = normalize_nested_workspace_wrapper_args(&effective_args, tool_stem);

    // L2 cold-build skip (issue #980): some rustc invocations cargo
    // issues through RUSTC_WRAPPER can NEVER hit the cache — `--print
    // sysroot` / `--print cfg` probes do not compile anything, and
    // `--emit=dep-info`-only runs only refresh the dep graph. Routing
    // those through zccache pays the full wrapper startup tax (process
    // spawn, daemon IPC) for guaranteed-zero benefit. Detect them here
    // and let the existing direct-exec tool-spawn path below handle
    // them instead of the zccache routing block.
    let zccache_routed_tool = routes_through_embedded_zccache(tool_stem);
    let non_cacheable = zccache_routed_tool && is_non_cacheable_rustc(&compile_args);
    if non_cacheable {
        tracing::debug!("soldr: {tool_stem} invocation is non-cacheable; bypassing zccache");
        profile.mark("non_cacheable_bypass");
    }

    // Route rustc-like compiler invocations through the daemon's
    // embedded zccache compile service. This includes `clippy-driver`
    // when cargo nests `RUSTC_WORKSPACE_WRAPPER=clippy-driver` inside
    // soldr's `RUSTC_WRAPPER` for workspace crates.
    if zccache_routed_tool && !non_cacheable && crate::cache_lib::cache_enabled_in_current_process()
    {
        // Test seam (SOLDR_TEST_ZCCACHE_BIN): when the fake-toolchain
        // integration tests set this env var, wrapper mode spawns the
        // named external `zccache wrapper <rustc> <args>` binary
        // instead of routing through the embedded daemon IPC.
        //
        // Why the seam exists: the fake-toolchain tests inspect a
        // tool.log file emitted by a bash-script fake zccache to
        // assert that wrapper-mode dispatch happened. The embedded
        // daemon path (compile_via_daemon → IPC → embedded compile
        // library) never spawns the fake_zccache binary, so it
        // cannot log the "zccache wrapper" line the tests check for.
        // The seam preserves the tests' original contract without
        // rewriting them to mock the daemon.
        //
        // Behaviorally identical to the daemon path in production
        // (the fake zccache invokes the real rustc); the difference
        // is only where the "zccache wrapper" line appears — an
        // external binary log vs. IPC message.
        if let Some(zccache_bin) =
            crate::binaries::non_empty_env_path(crate::TEST_ZCCACHE_BIN_ENV_VAR)
        {
            // Invoke the fake zccache with the historical wrapper-mode
            // subprocess contract: `<zccache> <rustc> <args>`. The
            // fake script's default arm reads `%~1`/`$1` as rustc,
            // logs its own `zccache wrapper cache_dir=...` line, then
            // execs rustc. The pre-embedded external zccache binary
            // took the same shape — no literal "wrapper" verb.
            profile.finish("before_test_override_spawn");
            let mut command = std::process::Command::new(&zccache_bin);
            command.args(&compile_args[1..]);
            let tool_path = resolve_wrapper_tool_path(&compile_args[1], tool_stem)?;
            crate::binaries::apply_resolved_toolchain_homes(&mut command, &tool_path);
            suppress_windows_console_window(&mut command);
            let status = command.status()?;
            return Ok(status.code().unwrap_or(1));
        }

        // L1 (issue #977 / #980 L1): dispatch the rustc invocation to
        // the daemon's embedded zccache service over IPC. The legacy
        // `zccache.exe` fork was deleted in the L1 second pass.
        profile.finish("before_embedded_compile_ipc");
        // soldr#1081 — lifted to `crate::compile_dispatch` so
        // multicall `zccache-soldr` dispatch can share the same
        // hang-safe retry logic. The bin-local copy below is the
        // legacy path retained only for the unit tests that still
        // import it; the production path now goes through the lifted
        // function.
        // soldr#1081 — lifted to `crate::compile_dispatch` so the
        // dedicated `zccache-soldr` shim binary can share the same
        // hang-safe retry logic.
        //
        // soldr#1300 — when the retry budget is exhausted and the
        // terminal condition is daemon UNAVAILABILITY (NotRunning /
        // spawn failure / transport error — NOT a compile failure or
        // error reply from a healthy daemon), degrade to the same
        // direct-exec path used for non-cacheable rustc invocations
        // below: run the real rustc uncached instead of failing the
        // build. A daemon-reported compile failure never reaches this
        // branch — it arrives as `Ok(exit_code != 0)` and is
        // propagated to cargo unchanged. `SOLDR_DAEMON_REQUIRED=1`
        // restores the pre-#1300 hard-fail for CI lanes that want to
        // catch daemon regressions.
        let result = match crate::compile_dispatch::compile_via_daemon_detailed(&compile_args[1..])
        {
            Ok(code) => Ok(code),
            Err(failure) if crate::compile_dispatch::should_fall_back_to_direct_rustc(&failure) => {
                crate::compile_dispatch::log_direct_exec_fallback_once(&failure);
                direct_exec_tool(tool_arg, tool_stem, &compile_args, None)
            }
            Err(failure) => Err(failure.into_soldr_error()),
        };
        return result;
    }

    direct_exec_tool(tool_arg, tool_stem, &compile_args, Some(profile))
}

/// Direct (uncached) exec of the wrapped tool — the non-daemon path.
/// Used for clippy-driver / non-cacheable rustc invocations, and as
/// the soldr#1300 degradation when the compile daemon is unavailable.
/// Preserves rustc's stdout/stderr/exit-code passthrough exactly: the
/// child inherits the wrapper's stdio and the exit code is returned
/// to cargo unchanged.
fn direct_exec_tool(
    tool_arg: &str,
    tool_stem: &str,
    effective_args: &[String],
    profile: Option<WrapperProfile>,
) -> Result<i32, SoldrError> {
    let tool_path = resolve_wrapper_tool_path(tool_arg, tool_stem)?;

    let mut command = std::process::Command::new(&tool_path);
    command.args(&effective_args[2..]);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &tool_path);
    suppress_windows_console_window(&mut command);
    if let Some(profile) = profile {
        profile.finish("before_tool_spawn");
    }
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

/// Resolve a wrapper compiler identity before deriving its toolchain homes.
/// Cargo may pass either an absolute path or a bare tool name, and every
/// wrapper execution path must classify that tool against the same path.
fn resolve_wrapper_tool_path(
    tool_arg: &str,
    tool_stem: &str,
) -> Result<std::path::PathBuf, SoldrError> {
    if std::path::Path::new(tool_arg).is_absolute() {
        Ok(tool_arg.into())
    } else {
        resolve_toolchain_binary(tool_stem)
    }
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
    let hex = zccache::hash::hash_bytes(bytes).to_hex();
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
//
// As of issue #1081 the `compile_via_daemon` body and the
// `is_compile_env_var` env filter were lifted into
// `crate::compile_dispatch` so multicall `zccache-soldr` dispatch can
// share them. The wrapper-entry dispatch site above now calls
// `compile_dispatch::compile_via_daemon` directly. Nothing remains in
// this file beyond the cargo-arg-shape predicates + stdin spill
// helper.

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
        let hash = zccache::hash::hash_bytes(bytes.as_bytes()).to_hex();
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

    crate::timed_test!(nested_workspace_wrapper_drops_real_compiler_path, {
        let workspace_tool = format!("{}-{}", "clippy", "driver");
        let argv = vec![
            "soldr".into(),
            format!("C:/toolchain/{workspace_tool}.exe"),
            "C:/toolchain/rustc.exe".into(),
            "--crate-name".into(),
            "demo".into(),
            "src/lib.rs".into(),
        ];

        let normalized = normalize_nested_workspace_wrapper_args(&argv, &workspace_tool);

        assert_eq!(
            normalized.as_ref(),
            &[
                "soldr",
                &format!("C:/toolchain/{workspace_tool}.exe"),
                "--crate-name",
                "demo",
                "src/lib.rs",
            ]
        );
    });

    crate::timed_test!(wrapper_normalization_preserves_other_argument_shapes, {
        let workspace_tool = format!("{}-{}", "clippy", "driver");
        let direct_workspace_argv = vec![
            "soldr".into(),
            workspace_tool.clone(),
            "--crate-name".into(),
            "demo".into(),
            "src/lib.rs".into(),
        ];
        let rustc_argv = wrapper_argv(&["--crate-name", "demo", "src/lib.rs"]);
        let compiler_named_source_argv = vec![
            "soldr".into(),
            workspace_tool.clone(),
            format!("{}.rs", WRAPPER_PASSTHROUGH_TOOLS[0]),
            "--crate-name".into(),
            "demo".into(),
        ];

        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&direct_workspace_argv, &workspace_tool),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&rustc_argv, "rustc"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize_nested_workspace_wrapper_args(&compiler_named_source_argv, &workspace_tool),
            std::borrow::Cow::Borrowed(_)
        ));
    });

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

    crate::timed_test!(clippy_driver_routes_through_embedded_zccache, {
        assert!(routes_through_embedded_zccache("rustc"));
        assert!(routes_through_embedded_zccache("clippy-driver"));
        assert!(!routes_through_embedded_zccache("rustfmt"));
        assert!(!routes_through_embedded_zccache("rustdoc"));
    });
}
