//! Compiler-exec phase: depfile prep, response file, pre-hash overlap, spawn.
//!
//! Runs on the miss path after the depgraph verdict has been logged and the
//! cached-hit branches have been exhausted. Returns the exec output plus the
//! per-phase timings consumed by the miss profiles.

use super::super::super::*;

pub(super) struct CompileExecRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) compiler: &'a NormalizedPath,
    pub(super) effective_args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) cwd_path: &'a NormalizedPath,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) compilation: &'a crate::compiler::CacheableCompilation,
    pub(super) dep_flags: &'a UserDepFlags,
    pub(super) rustc_args_opt: Option<&'a crate::depgraph::RustcParsedArgs>,
    pub(super) rustc_extern_paths: &'a [NormalizedPath],
    pub(super) is_rustc: bool,
    pub(super) client_env: &'a Option<Vec<(String, String)>>,
    pub(super) lineage: &'a crate::daemon::lineage::Lineage,
    pub(super) compile_start: std::time::Instant,
    pub(super) snap_clock: Clock,
}

/// Issue zccache#939 (step 1): how the compiler's stdout/stderr made it
/// back from the spawn helper into the pipeline.
///
/// `Buffered` mirrors the historical shape — `tokio::process::Output` was
/// `wait_with_output`-ed into two `Arc<Vec<u8>>`s. Used by:
///   * MSVC `/showIncludes` (stderr is parsed before being forwarded —
///     we need it in memory anyway).
///   * The multi-file C/C++ spawn path.
///   * `run_compiler_direct` (non-cacheable / fallback path).
///
/// `Streamed` is the rustc cold-path shape introduced by step 1: the
/// child's pipes were `tokio::io::copy`-ed straight into two on-disk
/// files under `state.depfile_tmpdir`. Step 2 will replace the
/// "synchronously read these files into Vec<u8>" boundary in
/// `pipeline/mod.rs` with a `rename` into the artifact store. For
/// step 1 the boundary keeps the downstream `Vec<u8>` API stable.
pub(super) enum CompileExecBytes {
    Buffered {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
    },
    Streamed {
        stdout_path: std::path::PathBuf,
        stderr_path: std::path::PathBuf,
        #[allow(dead_code)] // Read in step 2 — kept on the variant for future use.
        exit_code: i32,
        #[allow(dead_code)] // Read in step 2 — kept on the variant for future use.
        cached: bool,
    },
}

impl CompileExecBytes {
    /// Materialize the captured stdout as a `Vec<u8>`.
    ///
    /// For `Buffered` this clones the existing `Arc<Vec<u8>>` contents.
    /// For `Streamed` this performs a synchronous `std::fs::read` of the
    /// pending stdout file. Step 1 keeps the downstream `Vec<u8>` API
    /// unchanged so consumers don't have to learn the new shape; step 2
    /// will replace this `read` with a `rename` into the artifact store.
    /// A failed read returns an empty `Vec<u8>` and logs a warning — the
    /// caller already has the exit code and stderr-channel diagnostics
    /// to surface the compile failure.
    pub(super) fn stdout_bytes(&self) -> Vec<u8> {
        match self {
            CompileExecBytes::Buffered { stdout, .. } => (**stdout).clone(),
            CompileExecBytes::Streamed { stdout_path, .. } => {
                std::fs::read(stdout_path).unwrap_or_else(|e| {
                    tracing::warn!(
                        path = %stdout_path.display(),
                        "failed to read pending stdout file: {e}",
                    );
                    Vec::new()
                })
            }
        }
    }

    /// Materialize the captured stderr as a `Vec<u8>`. See [`stdout_bytes`]
    /// for the read-vs-clone semantics. The rustc cold path is the only
    /// `Streamed` producer today, so for MSVC `/showIncludes` callers the
    /// stderr already arrives as `Buffered`.
    pub(super) fn stderr_bytes(&self) -> Vec<u8> {
        match self {
            CompileExecBytes::Buffered { stderr, .. } => (**stderr).clone(),
            CompileExecBytes::Streamed { stderr_path, .. } => {
                std::fs::read(stderr_path).unwrap_or_else(|e| {
                    tracing::warn!(
                        path = %stderr_path.display(),
                        "failed to read pending stderr file: {e}",
                    );
                    Vec::new()
                })
            }
        }
    }
}

pub(super) struct CompileExecOutcome {
    pub(super) exit_code: i32,
    pub(super) bytes: CompileExecBytes,
    pub(super) depfile_strategy: DepfileStrategy,
    pub(super) show_includes_scan: Option<crate::depgraph::ScanResult>,
    pub(super) pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>>,
    pub(super) compiler_priority_decision: crate::daemon::process::CompilePriorityDecision,
    pub(super) pre_exec_ns: u64,
    pub(super) break_outputs_ns: u64,
    pub(super) compiler_process_ns: u64,
    pub(super) compiler_exec_ns: u64,
    pub(super) compiler_prep_ns: u64,
    pub(super) post_exec_ns: u64,
}

pub(super) enum CompileExecResult {
    Ok(CompileExecOutcome),
    Error(Response),
}

/// Atomic counter feeding the unique-id suffix on pending stdout/stderr
/// files in the streaming-to-disk path. Mirrors the `RSP_COUNTER` pattern
/// already used by `compiler::response_file` for the same "unique tmp
/// file under a shared dir" need. Single per-process counter is enough —
/// the same daemon owns every file under `state.depfile_tmpdir`.
static PENDING_OUTPUT_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn next_pending_id() -> u64 {
    PENDING_OUTPUT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Prepare depfile/response-file/output-paths, spawn the compiler, and gather
/// timings. The `pre_hash_task` returned is the rustc-only background hash of
/// source + externs (issue #532) — `await`ed later in the store phase so its
/// work overlaps with the compiler process itself.
pub(super) async fn run_compile_exec(req: CompileExecRequest<'_>) -> CompileExecResult {
    let CompileExecRequest {
        state_arc,
        compiler,
        effective_args,
        cwd,
        cwd_path,
        source_path,
        output_path,
        compilation,
        dep_flags,
        rustc_args_opt,
        rustc_extern_paths,
        is_rustc,
        client_env,
        lineage,
        compile_start,
        snap_clock,
    } = req;

    let state = state_arc.as_ref();

    // ── Phase: compiler exec (with depfile injection) ────────────────
    let pre_exec_ns = compile_start.elapsed().as_nanos() as u64;
    let t_exec = std::time::Instant::now();
    let supports_depfile = compilation.family.supports_depfile();
    let (mut extra_args, mut depfile_strategy) = crate::depgraph::depfile::prepare_depfile(
        supports_depfile,
        dep_flags,
        output_path,
        &state.depfile_tmpdir,
    );

    // For MSVC, use /showIncludes to get complete dependency info
    // (equivalent to depfiles for gcc/clang). This enables cache hits
    // for files with computed includes like `#include MACRO`.
    if compilation.family == crate::compiler::CompilerFamily::Msvc
        && depfile_strategy == DepfileStrategy::Unsupported
    {
        if !dep_flags.has_md {
            extra_args.push("/showIncludes".to_string());
        }
        depfile_strategy = DepfileStrategy::ShowIncludes;
    }

    // Combine expanded_args + extra_args for response-file length check.
    // Only allocates when extra_args is non-empty.
    let combined_args;
    let rsp_args: &[String] = if extra_args.is_empty() {
        effective_args
    } else {
        combined_args = [effective_args, extra_args.as_slice()].concat();
        &combined_args
    };

    let _rsp_guard = match crate::compiler::response_file::write_response_file_if_needed(
        rsp_args,
        &state.depfile_tmpdir,
        compilation.family,
    ) {
        Ok(guard) => guard,
        Err(e) => {
            return CompileExecResult::Error(Response::Error {
                message: format!("failed to write response file: {e}"),
            });
        }
    };

    let output_paths = if let Some(rustc_args) = rustc_args_opt {
        rustc_expected_output_paths(rustc_args, output_path, cwd_path)
    } else {
        vec![output_path.clone()]
    };
    let t_break_outputs = std::time::Instant::now();
    for path in &output_paths {
        if let Err(e) = break_output_hardlink_before_compile(path) {
            return CompileExecResult::Error(Response::Error {
                message: format!(
                    "failed to detach hardlinked output before compile {}: {e}",
                    path.display()
                ),
            });
        }
    }
    let break_outputs_ns = t_break_outputs.elapsed().as_nanos() as u64;

    let mut cmd = tokio::process::Command::new(compiler);
    if let Some(ref rsp) = _rsp_guard {
        cmd.arg(rsp.at_arg()).current_dir(cwd);
    } else {
        cmd.args(effective_args).current_dir(cwd);
        if !extra_args.is_empty() {
            cmd.args(&extra_args);
        }
    }
    apply_client_env(&mut cmd, client_env, lineage);
    let t_compiler_process = std::time::Instant::now();
    let is_link_like = rustc_args_opt
        .is_some_and(|rustc_args| rustc_args.emit_types.iter().any(|emit| emit == "link"));
    let compiler_priority =
        CompilePriority::from_client_env_for_link_like(client_env.as_deref(), is_link_like);
    let compiler_priority_decision = compiler_priority.resolve_for_current_load();

    // Issue #532: kick off hashing of pre-known inputs (source +
    // rustc_extern_paths) on a blocking thread, in parallel with the
    // rustc spawn. The 50-rlib externs of a workspace link dominate
    // hash_all_ns (~64 ms on a 4-core CI runner); overlapping them with
    // the ~38 ms rustc exec hides most of that cost. Late-arriving
    // include paths (from rustc's dep-info) are hashed post-compile and
    // merged with the pre-hash result. Skip for non-rustc compilers —
    // they don't have a known-ahead extern list, and their cold hash_all
    // is small anyway.
    let pre_hash_task: Option<tokio::task::JoinHandle<HashMap<NormalizedPath, ContentHash>>> =
        if is_rustc && !rustc_extern_paths.is_empty() {
            let pre_state = Arc::clone(state_arc);
            let pre_source = source_path.clone();
            let pre_externs: Vec<NormalizedPath> = rustc_extern_paths.to_vec();
            let pre_clock = snap_clock;
            Some(tokio::task::spawn_blocking(move || {
                use rayon::prelude::*;
                let all_paths: Vec<&NormalizedPath> = std::iter::once(&pre_source)
                    .chain(pre_externs.iter())
                    .collect();
                all_paths
                    .par_iter()
                    .filter_map(|path| {
                        let hash_path = resolve_pch_source(path, &pre_state.pch_source_map)
                            .unwrap_or_else(|| (*path).clone());
                        hash_file(&pre_state.cache_system, &hash_path, pre_clock)
                            .ok()
                            .map(|h| ((*path).clone(), h))
                    })
                    .collect()
            }))
        } else {
            None
        };

    // Issue #813 / #816: acquire a compile-concurrency permit before
    // spawning the compiler. The semaphore (when present — None means
    // ZCCACHE_MAX_PARALLEL_COMPILES=0 opt-out) gates total in-flight
    // compiler children across ALL clients sharing this daemon.
    // Permit is held for the duration of the spawn + wait; drops on
    // scope exit, freeing the slot for the next queued request.
    //
    // The `compile_start` / `compile_end` log events are deliberately
    // structured so an integration test (sub-task #817) can parse the
    // log and assert no two compile intervals overlap when the cap is
    // 1 (sub-task #5 of the meta).
    let client_pid = lineage.client_pid.unwrap_or(0);
    let _permit = if let Some(sem) = state.compile_concurrency.as_ref() {
        let available_before = sem.available_permits();
        let permit = Arc::clone(sem).acquire_owned().await.ok();
        tracing::info!(
            event = "compile_start",
            client_pid,
            available_before,
            "compile_start client_pid={client_pid} available_before={available_before}",
        );
        permit
    } else {
        None
    };
    let compile_span_start = std::time::Instant::now();

    // Issue zccache#939 step 1: branch the spawn helper on whether this
    // invocation is rustc-streamable. The rustc cold path (the only one
    // we actually need to drop foreground bytes for in step 2) pipes
    // stdout+stderr straight to two on-disk files under
    // `state.depfile_tmpdir` via `tokio_command_streaming_to_files`.
    // MSVC `/showIncludes` MUST stay buffered — `parse_show_includes`
    // runs in this same task and consumes the stderr `&[u8]`. The
    // multi-file C/C++ path and `run_compiler_direct` do their own
    // spawns; this site only governs the single-file pipeline branch.
    //
    // Classification helper: `CompilerFamily` (compiler/mod.rs) is the
    // canonical detection seam. `compilation.family == Rustc` is the
    // single-bit predicate. `is_rustc` is already in scope but carries
    // the same information through the request struct — preferring
    // `compilation.family` keeps the predicate next to the enum it
    // matches and survives the eventual broker / multi-handler split.
    let stream_to_disk =
        compilation.family == crate::compiler::CompilerFamily::Rustc
            && depfile_strategy != DepfileStrategy::ShowIncludes;

    enum SpawnResult {
        Buffered(std::process::Output),
        Streamed {
            exit: std::process::ExitStatus,
            stdout_path: std::path::PathBuf,
            stderr_path: std::path::PathBuf,
        },
    }

    let spawn_io_result: std::io::Result<SpawnResult> = if stream_to_disk {
        let pending_id = next_pending_id();
        let stdout_path = state
            .depfile_tmpdir
            .as_path()
            .join(format!("compile-{pending_id}.stdout"));
        let stderr_path = state
            .depfile_tmpdir
            .as_path()
            .join(format!("compile-{pending_id}.stderr"));
        let stdout_path_clone = stdout_path.clone();
        let stderr_path_clone = stderr_path.clone();
        crate::daemon::process::tokio_command_streaming_to_files(
            &mut cmd,
            compiler_priority_decision.effective,
            stdout_path,
            stderr_path,
        )
        .await
        .map(|exit| SpawnResult::Streamed {
            exit,
            stdout_path: stdout_path_clone,
            stderr_path: stderr_path_clone,
        })
    } else {
        crate::daemon::process::tokio_command_output_with_priority(
            &mut cmd,
            compiler_priority_decision.effective,
        )
        .await
        .map(SpawnResult::Buffered)
    };
    let compiler_process_ns = t_compiler_process.elapsed().as_nanos() as u64;

    if state.compile_concurrency.is_some() {
        let duration_ns = compile_span_start.elapsed().as_nanos() as u64;
        let exit_code = match spawn_io_result.as_ref() {
            Ok(SpawnResult::Buffered(o)) => o.status.code().unwrap_or(-1),
            Ok(SpawnResult::Streamed { exit, .. }) => exit.code().unwrap_or(-1),
            Err(_) => -1,
        };
        tracing::info!(
            event = "compile_end",
            client_pid,
            duration_ns,
            exit_code,
            "compile_end client_pid={client_pid} duration_ns={duration_ns} exit_code={exit_code}",
        );
    }

    let spawn_result = match spawn_io_result {
        Ok(r) => r,
        Err(e) => {
            return CompileExecResult::Error(Response::Error {
                message: format!("failed to run compiler: {e}"),
            });
        }
    };
    let compiler_exec_ns = t_exec.elapsed().as_nanos() as u64;
    let compiler_prep_ns = compiler_exec_ns.saturating_sub(compiler_process_ns);

    let t_post_exec = std::time::Instant::now();
    let (exit_code, bytes, show_includes_scan) = match spawn_result {
        SpawnResult::Buffered(output) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = Arc::new(output.stdout);
            // For MSVC /showIncludes: parse dependency info from stderr and
            // filter out the /showIncludes lines before returning to the client.
            let (show_includes_scan, stderr_bytes) =
                if depfile_strategy == DepfileStrategy::ShowIncludes {
                    let (scan, filtered) = crate::depgraph::show_includes::parse_show_includes(
                        &output.stderr,
                        source_path,
                        cwd_path,
                    );
                    (Some(scan), filtered)
                } else {
                    (None, output.stderr)
                };
            let stderr = Arc::new(stderr_bytes);
            (
                exit_code,
                CompileExecBytes::Buffered { stdout, stderr },
                show_includes_scan,
            )
        }
        SpawnResult::Streamed {
            exit,
            stdout_path,
            stderr_path,
        } => {
            let exit_code = exit.code().unwrap_or(-1);
            // Rustc never produces /showIncludes output, so the scan is
            // always `None` on this branch — the predicate above
            // (`depfile_strategy != ShowIncludes`) is in fact redundant
            // for the rustc family, but we keep both checks so a future
            // hypothetical "rustc with /showIncludes" wouldn't silently
            // skip parsing.
            (
                exit_code,
                CompileExecBytes::Streamed {
                    stdout_path,
                    stderr_path,
                    exit_code,
                    cached: false,
                },
                None,
            )
        }
    };
    let post_exec_ns = t_post_exec.elapsed().as_nanos() as u64;

    // Drop the response-file guard now that the compiler has exited. The
    // pre-split function held the guard until end-of-function via `let
    // _rsp_guard = ...`; keeping it bound to a local in this helper does
    // the same — the guard drops when `run_compile_exec` returns, which is
    // before any subsequent post-exec work touches the response file.
    drop(_rsp_guard);

    CompileExecResult::Ok(CompileExecOutcome {
        exit_code,
        bytes,
        depfile_strategy,
        show_includes_scan,
        pre_hash_task,
        compiler_priority_decision,
        pre_exec_ns,
        break_outputs_ns,
        compiler_process_ns,
        compiler_exec_ns,
        compiler_prep_ns,
        post_exec_ns,
    })
}
