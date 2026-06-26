//! Cold-miss artifact storage for compile requests.

use super::super::*;

/// Issue zccache#939 step 2: how the compiler's stdout/stderr are sourced
/// when the success path stores a miss artifact. Mirrors
/// `CompileExecBytes` from `pipeline::compile_exec` but is owned by the
/// store path so the pipeline boundary can hand the value in by-value
/// (taking the pending paths with it).
///
/// `Buffered` — bytes are already in memory; used by the MSVC
/// `/showIncludes` path, the multi-file C/C++ path, and any caller that
/// materialized the bytes itself (e.g. the failure path that needed
/// stderr to build a `Response::CompileResult`). The store path stores
/// these Arcs inline into the `ArtifactIndex`.
///
/// `Streamed` — bytes live on disk under `state.depfile_tmpdir` and the
/// caller has NOT read them. The store path renames the pending files
/// into the per-key cache slots under `state.artifact_dir` and reads
/// them ONCE to build the `Arc<Vec<u8>>` needed for the inline
/// `ArtifactIndex` field and the `Response::CompileResult` reply. Before
/// step 2 the pipeline boundary read them upfront (Step 1's hack), so
/// the streamed cold path paid `read-pending` + `inline-clone` + would
/// have paid a separate `write-cache-slot` if a Step 3 ever wrote the
/// slot. Step 2 collapses that to `rename-pending-into-slot` +
/// `single-read-from-slot`.
pub(super) enum CompileExecStdioSource {
    Buffered {
        stdout: Arc<Vec<u8>>,
        stderr: Arc<Vec<u8>>,
    },
    Streamed {
        stdout_path: std::path::PathBuf,
        stderr_path: std::path::PathBuf,
    },
}

/// Resolved stdout/stderr bytes plus a flag for whether the source was
/// `Streamed` (renamed into the cache slot) vs `Buffered` (passed through).
/// Returned by `store_miss_artifact` so the pipeline can forward the
/// SAME `Arc`s into `Response::CompileResult` without a second read.
pub(super) struct ResolvedStdio {
    pub(super) stdout: Arc<Vec<u8>>,
    pub(super) stderr: Arc<Vec<u8>>,
}

pub(super) struct MissArtifactStoreRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) sid: &'a SessionId,
    pub(super) context_key: &'a ContextKey,
    pub(super) source_path: &'a NormalizedPath,
    pub(super) output_path: &'a NormalizedPath,
    pub(super) scan_result: crate::depgraph::ScanResult,
    pub(super) hash_map: &'a HashMap<NormalizedPath, ContentHash>,
    pub(super) output_data: Vec<u8>,
    /// Issue #643: when the user's compile line emitted a depfile that
    /// downstream build tools depend on (`-MD -MF <path>` or `-MD` with
    /// the implicit `<output>.d`), the post-compile depfile bytes are
    /// captured here so the cache hit can restore the depfile alongside
    /// the object. `None` for compiles without user depfile flags, for
    /// MSVC `/showIncludes` (parsed from stderr, not on disk), and for
    /// rustc (separate persist path).
    pub(super) user_depfile: Option<(NormalizedPath, Vec<u8>)>,
    pub(super) rustc_all_outputs: Option<&'a [RustcOutputFile]>,
    /// Issue zccache#939 step 2: stdout/stderr enter the store path
    /// either pre-buffered (MSVC `/showIncludes`, multi-file, failure
    /// fallback) or as on-disk pending paths the streamed rustc spawn
    /// just wrote. The store path owns the resolution (rename + read
    /// for streamed; clone-Arc for buffered) so the pipeline boundary
    /// never has to materialize twice.
    pub(super) stdio_source: CompileExecStdioSource,
    pub(super) exit_code: i32,
    pub(super) compile_start: Instant,
}

#[derive(Default)]
pub(super) struct MissArtifactStoreStats {
    pub(super) artifact_store_ns: u64,
    pub(super) depgraph_update_ns: u64,
    pub(super) artifact_build_ns: u64,
    pub(super) persist_enqueue_ns: u64,
    pub(super) artifact_insert_stats_ns: u64,
    pub(super) artifact_meta_build_ns: u64,
    pub(super) rust_snapshot_ns: u64,
    pub(super) rust_snapshot_hardlink_count: u64,
    pub(super) rust_snapshot_copy_count: u64,
    pub(super) rust_snapshot_copy_bytes: u64,
    pub(super) rust_snapshot_error_count: u64,
    pub(super) artifact_index_build_ns: u64,
    pub(super) artifact_index_persist_ns: u64,
    pub(super) artifact_memory_insert_ns: u64,
    /// Issue zccache#939 step 2: wall time spent renaming the pending
    /// stdout/stderr files into the per-key cache slots, plus the
    /// single read used to populate the inline `ArtifactIndex` fields.
    /// Zero for the `Buffered` path (no rename, no read).
    pub(super) stdio_rename_ns: u64,
}

/// Issue zccache#939 step 2: cache slot path for a pending stdout
/// payload that was streamed straight to disk. Lives at the artifact
/// dir root next to the existing `{key}_<i>` payload files so a future
/// disk-fallback path can resolve it from `key_hex` alone.
pub(super) fn pending_stdout_cache_path(artifact_dir: &Path, key_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{key_hex}.stdout"))
}

/// Companion to [`pending_stdout_cache_path`].
pub(super) fn pending_stderr_cache_path(artifact_dir: &Path, key_hex: &str) -> PathBuf {
    artifact_dir.join(format!("{key_hex}.stderr"))
}

/// Result of the miss-store path: per-phase stats plus the resolved
/// stdio Arcs the pipeline forwards into `Response::CompileResult`.
///
/// Issue zccache#939 step 2: the `Streamed` arm of
/// `CompileExecStdioSource` lands the bytes in the cache slot via
/// rename and then reads them back ONCE to populate the inline
/// `ArtifactIndex` fields; the same `Arc` is returned here so the
/// response uses zero additional disk reads. The `Buffered` arm just
/// forwards the input Arcs unchanged.
///
/// **Idempotency**: the artifact-index entry is published (via
/// `index_writer_tx.send` for rustc outputs at line ~290; via the
/// foreground `state.artifacts.insert` for single-output cc/cpp at
/// line ~360) AFTER the stdio rename completes, so a concurrent
/// wrapper that races the same key sees either "no entry yet" (and
/// recompiles) or "entry present and slot files exist" — never an
/// entry pointing at unrenamed pending files. See the
/// `pending_writes::register` / `complete` pair below for the
/// in-flight-publication signal that lets concurrent lookups
/// optionally wait instead of recompiling.
pub(super) struct MissArtifactStoreOutcome {
    pub(super) stats: MissArtifactStoreStats,
    pub(super) resolved_stdio: ResolvedStdio,
}

pub(super) fn store_miss_artifact(
    request: MissArtifactStoreRequest<'_>,
) -> MissArtifactStoreOutcome {
    let MissArtifactStoreRequest {
        state_arc,
        sid,
        context_key,
        source_path,
        output_path,
        scan_result,
        hash_map,
        output_data,
        user_depfile,
        rustc_all_outputs,
        stdio_source,
        exit_code,
        compile_start,
    } = request;
    let state = state_arc.as_ref();
    let t_store = Instant::now();
    let get_hash = |p: &Path| {
        let path = NormalizedPath::new(p);
        hash_map.get(&path).copied()
    };
    let include_count = scan_result.resolved.len();
    let t_depgraph_update = Instant::now();
    let artifact_key_result = state
        .dep_graph
        .load()
        .update(context_key, scan_result, get_hash);
    let mut stats = MissArtifactStoreStats {
        depgraph_update_ns: t_depgraph_update.elapsed().as_nanos() as u64,
        ..MissArtifactStoreStats::default()
    };

    if let Some(artifact_key) = artifact_key_result {
        let artifact_key_hex = artifact_key.hash().to_hex();
        let ctx_hex = &context_key.hash().to_hex()[..8];
        write_session_log(
            &state.sessions,
            sid,
            &format!(
                "[DIAG] update: {} ctx={ctx_hex} artifact_key={} includes={include_count}",
                source_path.display(),
                &artifact_key_hex[..8],
            ),
        );

        record_pch_source_mapping(state, source_path, output_path);

        // Issue zccache#939 step 2: resolve stdio BEFORE the
        // index-publication path so the rename completes (and the slot
        // files are durable on disk) before the concurrent-lookup
        // visibility surface is widened by `index_writer_tx.send` /
        // `state.artifacts.insert`. The artifact key is now known —
        // safe to name the cache slot.
        let t_stdio = Instant::now();
        let resolved_stdio = resolve_stdio_into_cache_slot(
            state,
            sid,
            &artifact_key_hex,
            stdio_source,
        );
        stats.stdio_rename_ns = t_stdio.elapsed().as_nanos() as u64;

        let t_artifact_build = Instant::now();
        if let Some(all_outputs) = rustc_all_outputs {
            store_rustc_outputs(
                state_arc,
                sid,
                source_path,
                all_outputs,
                &artifact_key_hex,
                &resolved_stdio.stdout,
                &resolved_stdio.stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
            );
        } else {
            store_single_output(
                state_arc,
                sid,
                source_path,
                output_path,
                output_data,
                user_depfile,
                &artifact_key_hex,
                &resolved_stdio.stdout,
                &resolved_stdio.stderr,
                exit_code,
                compile_start,
                &mut stats,
                t_artifact_build,
            );
        }
        stats.artifact_store_ns = t_store.elapsed().as_nanos() as u64;
        return MissArtifactStoreOutcome {
            stats,
            resolved_stdio,
        };
    }

    // No artifact key produced — depgraph update declined to store
    // (e.g. unresolved includes). Fall back to materializing the stdio
    // from the original source (no rename happens) so the caller can
    // still build a `Response::CompileResult`. This path doesn't widen
    // the visibility surface, so there's no race to worry about.
    let t_stdio = Instant::now();
    let resolved_stdio = resolve_stdio_without_cache_slot(stdio_source);
    stats.stdio_rename_ns = t_stdio.elapsed().as_nanos() as u64;
    stats.artifact_store_ns = t_store.elapsed().as_nanos() as u64;
    MissArtifactStoreOutcome {
        stats,
        resolved_stdio,
    }
}

/// Resolve `CompileExecStdioSource::Streamed` by renaming the pending
/// files into the per-key cache slots and then reading them back once
/// to build the `Arc<Vec<u8>>` the inline `ArtifactIndex` field and the
/// `Response::CompileResult` reply both need. A rename failure (rare:
/// cross-volume + cross-volume hardlink + cross-fs copy all failed)
/// degrades gracefully to "just read the pending file" so the response
/// stays well-formed; the slot is left empty in that case.
///
/// The `Buffered` arm is a pure pass-through.
fn resolve_stdio_into_cache_slot(
    state: &SharedState,
    sid: &SessionId,
    artifact_key_hex: &str,
    source: CompileExecStdioSource,
) -> ResolvedStdio {
    match source {
        CompileExecStdioSource::Buffered { stdout, stderr } => ResolvedStdio { stdout, stderr },
        CompileExecStdioSource::Streamed {
            stdout_path,
            stderr_path,
        } => {
            let artifact_dir = state.artifact_dir.as_path();
            let stdout_slot = pending_stdout_cache_path(artifact_dir, artifact_key_hex);
            let stderr_slot = pending_stderr_cache_path(artifact_dir, artifact_key_hex);
            let stdout = move_and_read(&stdout_path, &stdout_slot, state, sid, "stdout");
            let stderr = move_and_read(&stderr_path, &stderr_slot, state, sid, "stderr");
            ResolvedStdio {
                stdout: Arc::new(stdout),
                stderr: Arc::new(stderr),
            }
        }
    }
}

/// No artifact key was produced (depgraph declined) OR the caller
/// short-circuited before a key could be derived (e.g. an
/// output-collection failure). For the streamed arm we still need to
/// surface the bytes to the caller's response and clean up the pending
/// files — there's nowhere stable to rename them to, so this just
/// reads-then-removes. Exposed to `pipeline::store_outcome` for the
/// output-collection-failure short-circuit.
pub(super) fn resolve_stdio_without_cache_slot(source: CompileExecStdioSource) -> ResolvedStdio {
    match source {
        CompileExecStdioSource::Buffered { stdout, stderr } => ResolvedStdio { stdout, stderr },
        CompileExecStdioSource::Streamed {
            stdout_path,
            stderr_path,
        } => {
            let stdout = std::fs::read(&stdout_path).unwrap_or_default();
            let stderr = std::fs::read(&stderr_path).unwrap_or_default();
            let _ = std::fs::remove_file(&stdout_path);
            let _ = std::fs::remove_file(&stderr_path);
            ResolvedStdio {
                stdout: Arc::new(stdout),
                stderr: Arc::new(stderr),
            }
        }
    }
}

/// Try to rename `pending_path` into `cache_slot`, then read the slot
/// bytes for the inline `ArtifactIndex` / response. If the rename
/// itself fails, fall back to reading the pending file in place so the
/// caller still sees the bytes; the cache slot just stays absent in
/// that case.
fn move_and_read(
    pending_path: &Path,
    cache_slot: &Path,
    state: &SharedState,
    sid: &SessionId,
    which: &str,
) -> Vec<u8> {
    match persist_pending_pipe(pending_path, cache_slot) {
        Ok(_) => std::fs::read(cache_slot).unwrap_or_else(|e| {
            tracing::warn!(
                key = %cache_slot.display(),
                "failed to read cache slot {which} after rename: {e}"
            );
            Vec::new()
        }),
        Err(e) => {
            tracing::warn!(
                pending = %pending_path.display(),
                slot = %cache_slot.display(),
                "failed to move pending {which} into cache slot, falling back to read: {e}"
            );
            write_session_log(
                &state.sessions,
                sid,
                &format!(
                    "[DIAG] streamed_stdio_rename_failed: which={which} pending={} slot={} error={e}",
                    pending_path.display(),
                    cache_slot.display(),
                ),
            );
            let bytes = std::fs::read(pending_path).unwrap_or_default();
            let _ = std::fs::remove_file(pending_path);
            bytes
        }
    }
}

fn record_pch_source_mapping(
    state: &SharedState,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
) {
    if let Some(ext) = output_path.extension() {
        if ext == "pch" || ext == "gch" {
            state
                .pch_source_map
                .insert(output_path.clone(), source_path.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn store_rustc_outputs(
    state_arc: &Arc<SharedState>,
    sid: &SessionId,
    source_path: &NormalizedPath,
    all_outputs: &[RustcOutputFile],
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
) {
    let state = state_arc.as_ref();
    let t_artifact_meta_build = Instant::now();
    // Issue #629: the prior four-pass shape (`.iter().map().sum()`
    // + three `.iter().map().collect()`s) walks `all_outputs` four
    // times and allocates three Vecs whose capacity wasn't hinted.
    // For the typical rustc miss (2 outputs: `.rmeta` + `.rlib`) the
    // savings are micro, but every µs on the daemon's
    // response-return critical path stacks against the same-job-seed
    // warm gap soldr is chasing in #629. Single pass with
    // `with_capacity` hint and a `saturating_add` accumulator.
    let n = all_outputs.len();
    let mut output_names: Vec<String> = Vec::with_capacity(n);
    let mut output_sizes: Vec<u64> = Vec::with_capacity(n);
    let mut source_paths: Vec<NormalizedPath> = Vec::with_capacity(n);
    let mut artifact_bytes: u64 = 0;
    for output in all_outputs {
        output_names.push(output.name.clone());
        output_sizes.push(output.size);
        source_paths.push(output.path.clone());
        artifact_bytes = artifact_bytes.saturating_add(output.size);
    }
    stats.artifact_meta_build_ns = t_artifact_meta_build.elapsed().as_nanos() as u64;

    // Rustc outputs are already on disk under target/. Persist them before
    // publishing the in-memory artifact so depgraph hits never point at a
    // key whose payload files have not landed yet.
    let t_artifact_index_build = Instant::now();
    let meta = ArtifactIndex::new(
        output_names,
        output_sizes,
        Arc::clone(stdout),
        Arc::clone(stderr),
        exit_code,
    );
    stats.artifact_index_build_ns = t_artifact_index_build.elapsed().as_nanos() as u64;
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;

    let t_persist_sync = Instant::now();
    let sync_persist_result =
        persist_artifact_paths_with_stats(&state.artifact_dir, artifact_key_hex, &source_paths);
    stats.rust_snapshot_ns = t_persist_sync.elapsed().as_nanos() as u64;
    let persisted = match sync_persist_result {
        Ok(snapshot_stats) => {
            stats.rust_snapshot_hardlink_count = snapshot_stats.hardlink_count;
            stats.rust_snapshot_copy_count = snapshot_stats.copy_count;
            stats.rust_snapshot_copy_bytes = snapshot_stats.copy_bytes;
            let _ = state
                .index_writer_tx
                .send((artifact_key_hex.to_string(), meta.clone()));
            true
        }
        Err(e) => {
            stats.rust_snapshot_error_count = stats.rust_snapshot_error_count.saturating_add(1);
            tracing::warn!(
                key = %artifact_key_hex,
                "failed to synchronously persist rustc artifact outputs: {e}"
            );
            write_session_log(
                &state.sessions,
                sid,
                &format!("[DIAG] rustc_persist_failed: key={artifact_key_hex} error={e}"),
            );
            false
        }
    };

    stats.persist_enqueue_ns = 0;

    let t_artifact_insert_stats = Instant::now();
    if persisted {
        let t_artifact_memory_insert = Instant::now();
        let cached = CachedArtifact::from_index(meta);
        state.artifacts.insert(artifact_key_hex.to_string(), cached);
        stats.artifact_memory_insert_ns = t_artifact_memory_insert.elapsed().as_nanos() as u64;
    }

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
}

#[allow(clippy::too_many_arguments)]
fn store_single_output(
    state_arc: &Arc<SharedState>,
    sid: &SessionId,
    source_path: &NormalizedPath,
    output_path: &NormalizedPath,
    output_data: Vec<u8>,
    user_depfile: Option<(NormalizedPath, Vec<u8>)>,
    artifact_key_hex: &str,
    stdout: &Arc<Vec<u8>>,
    stderr: &Arc<Vec<u8>>,
    exit_code: i32,
    compile_start: Instant,
    stats: &mut MissArtifactStoreStats,
    t_artifact_build: Instant,
) {
    let state = state_arc.as_ref();
    // Issue #643: stash the user's depfile as a second output so cache
    // hits can restore it alongside the object. Only `UserSpecified` /
    // `UserDefault` strategies reach this site with `Some(_)` — the
    // pipeline filters out the `Injected` strategy (zccache injected
    // the file purely for its own depgraph use; the user didn't ask
    // for it on disk) and MSVC `/showIncludes` (no on-disk depfile to
    // begin with). The cached `name` is the depfile basename; the
    // destination on hit is supplied independently by the caller (the
    // current build's `-MF` value), so artifacts remain reusable
    // across renamed-output workspaces.
    let mut outputs = vec![ArtifactOutput {
        name: output_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        payload: ArtifactPayload::Bytes(Arc::new(output_data)),
    }];
    let depfile_source_path: Option<NormalizedPath> = user_depfile.as_ref().map(|(p, _)| p.clone());
    if let Some((dep_path, dep_bytes)) = user_depfile {
        outputs.push(ArtifactOutput {
            name: dep_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            payload: ArtifactPayload::Bytes(Arc::new(dep_bytes)),
        });
    }
    let artifact = ArtifactData {
        outputs,
        stdout: Arc::clone(stdout),
        stderr: Arc::clone(stderr),
        exit_code,
    };

    let artifact_bytes: u64 = artifact
        .outputs
        .iter()
        .map(|o| o.payload.size_bytes())
        .sum();
    let cached = CachedArtifact::from_artifact_data(&artifact);
    stats.artifact_build_ns = t_artifact_build.elapsed().as_nanos() as u64;
    let t_persist_enqueue = Instant::now();

    let artifact_dir = state.artifact_dir.clone();
    let key_hex = artifact_key_hex.to_string();
    let persist_meta = cached.meta.clone();
    let mut source_paths: Vec<NormalizedPath> = vec![output_path.clone()];
    if let Some(dep_path) = depfile_source_path {
        source_paths.push(dep_path);
    }
    let payload_size: usize = artifact
        .outputs
        .iter()
        .map(|o| o.payload.size_bytes() as usize)
        .sum();
    state
        .in_flight_bytes
        .fetch_add(payload_size, Ordering::Relaxed);
    let guard = InFlightGuard {
        state: Arc::clone(state_arc),
        size: payload_size,
    };
    let sem = Arc::clone(&state.persist_semaphore);
    let state_ref = Arc::clone(state_arc);
    let completion_key = artifact_key_hex.to_string();
    // Issue #610, DD-025 condition 1: pending-write registration around
    // the C/C++ cold-miss persist spawn. Concurrent lookups can observe
    // that disk publication is in flight and (optionally) wait briefly
    // for it instead of recompiling-on-race. Completion is signalled on
    // both success and failure paths (failure wakes waiters → re-lookup
    // misses → recompile; the DD-025 failure-mode-is-miss invariant).
    let _pending = pending_writes::register(&state.pending_cache_writes, artifact_key_hex);
    tokio::spawn(async move {
        let _permit = sem.acquire().await.unwrap();
        let written = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            // Issue #728: `gap_ms` = wall-clock between
            // "linker-success-recorded" (immediately before this spawn was
            // scheduled) and "persist-attempt-started" (now, inside the
            // blocking task). Captured *before* the persist call so the
            // measurement excludes the persist work itself; useful for
            // distinguishing "queue starvation under burst load" from
            // "src vanished" / errno-N failure modes (the rest of the
            // diagnostic — src=, dst=, errno=, src_exists_now=,
            // src_size_now= — is baked into the error by
            // `persist::enrich_persist_err`).
            let gap_ms = t_persist_enqueue.elapsed().as_millis() as u64;
            if let Err(e) = persist_artifact_paths(&artifact_dir, &key_hex, &source_paths) {
                tracing::warn!(
                    key = %key_hex,
                    gap_ms,
                    "failed to persist artifact output: {e}"
                );
            }
            (key_hex, persist_meta)
        })
        .await;
        if let Ok((key_hex, meta)) = written {
            let _ = state_ref.index_writer_tx.send((key_hex, meta));
        }
        // Always complete the pending entry, even on JoinError, so
        // waiters cannot hang past the spawn's lifetime.
        pending_writes::complete(&state_ref.pending_cache_writes, &completion_key);
    });
    stats.persist_enqueue_ns = t_persist_enqueue.elapsed().as_nanos() as u64;

    let t_artifact_insert_stats = Instant::now();
    state.artifacts.insert(artifact_key_hex.to_string(), cached);

    let latency_ns = compile_start.elapsed().as_nanos() as u64;
    state.stats.record_miss(latency_ns, artifact_bytes);
    let src = source_path.clone();
    record_session_stat(&state.sessions, sid, move |t| {
        t.record_miss(src, artifact_bytes);
    });
    stats.artifact_insert_stats_ns = t_artifact_insert_stats.elapsed().as_nanos() as u64;
}
