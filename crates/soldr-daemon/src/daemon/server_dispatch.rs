async fn handle_connection<S>(
    mut stream: S,
    state: Arc<State>,
    peer: crate::daemon::ipc_peer::PeerIdentity,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use running_process::broker::backend_sdk::MuxPoll;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    let mux = soldr_backend_endpoint_mux(state.daemon_identity.clone());
    let mut prefix = [0_u8; CONTROL_FRAME_HEADER_BYTES];
    if !matches!(
        timeout(HANDSHAKE_READ_TIMEOUT, stream.read_exact(&mut prefix)).await,
        Ok(Ok(_))
    ) {
        return Ok(());
    }
    let mut buffered = prefix.to_vec();
    loop {
        match mux.poll(&buffered) {
            Ok(MuxPoll::Legacy) => break,
            Ok(MuxPoll::NeedMoreBytes) => {
                let mut chunk = [0_u8; 4096];
                let read = match timeout(HANDSHAKE_READ_TIMEOUT, stream.read(&mut chunk)).await {
                    Ok(Ok(0)) | Err(_) => return Ok(()),
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => return Ok(()),
                };
                buffered.extend_from_slice(&chunk[..read]);
            }
            Ok(MuxPoll::ProbeAnswered { reply, .. }) => {
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.write_all(&reply)).await;
                let _ = timeout(HANDSHAKE_READ_TIMEOUT, stream.flush()).await;
                return Ok(());
            }
            Ok(MuxPoll::Payload { .. }) | Err(_) => {
                // #1853: never drop a socket with the peer's bytes still
                // queued — see `drain_then_close`.
                drain_then_close(&mut stream).await;
                return Ok(());
            }
        }
    }

    // #1853: check the peer's version explicitly rather than letting the
    // decode fail. A version mismatch is a known, diagnosable condition, and
    // reporting it as an opaque transport reset is what made this cost a day
    // to attribute downstream.
    if buffered.len() >= CONTROL_FRAME_HEADER_BYTES {
        let peer_version = u32::from_le_bytes(
            buffered[4..CONTROL_FRAME_HEADER_BYTES]
                .try_into()
                .expect("slice of CONTROL_FRAME_HEADER_BYTES-4 bytes is always 4 bytes wide"),
        );
        if peer_version != PROTOCOL_VERSION {
            reject_version_mismatch(&mut stream, peer_version).await;
            return Ok(());
        }
    }

    let req: Request = match read_frame_async_with_prefix(&mut stream, &buffered).await {
        Ok(r) => r,
        Err(error) => {
            tracing::debug!(%error, "soldr-daemon: dropping undecodable IPC frame");
            drain_then_close(&mut stream).await;
            return Ok(());
        }
    };
    state.request_count.fetch_add(1, Ordering::Relaxed);
    state.touch_activity();
    match req {
        Request::ListTargetRegistry => {
            let db_path = state.db_path.clone();
            let response = tokio::task::spawn_blocking(move || {
                TargetRegistry::open(&db_path)
                    .and_then(|registry| registry.list())
                    .map(|rows| {
                        Response::TargetRegistryRows(
                            rows.into_iter()
                                .map(|row| crate::daemon::protocol::TargetRegistryRow {
                                    path: row.path.display().to_string(),
                                    last_used: row.last_used,
                                })
                                .collect(),
                        )
                    })
                    .unwrap_or_else(|error| {
                        Response::Error(format!("list target registry: {error}"))
                    })
            })
            .await
            .unwrap_or_else(|error| Response::Error(format!("list target registry task: {error}")));
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::RemoveTargetRegistry { paths } => {
            let db_path = state.db_path.clone();
            let response = tokio::task::spawn_blocking(move || {
                let paths = paths
                    .into_iter()
                    .map(std::path::PathBuf::from)
                    .collect::<Vec<_>>();
                TargetRegistry::open(&db_path)
                    .and_then(|registry| registry.remove_many(&paths))
                    .map(|removed| Response::TargetRegistryRemoved {
                        removed: removed as u32,
                    })
                    .unwrap_or_else(|error| {
                        Response::Error(format!("remove target registry rows: {error}"))
                    })
            })
            .await
            .unwrap_or_else(|error| {
                Response::Error(format!("remove target registry task: {error}"))
            });
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::AcquireResidentCapacity { permits } => {
            if permits == 0 {
                let _ = write_frame_async(
                    &mut stream,
                    &Response::Error(
                        "resident capacity requires at least one permit".to_string(),
                    ),
                )
                .await;
                return Ok(());
            }

            // The embedded service owns the canonical compile-capacity
            // semaphore. Retaining this opaque guard in the connection task
            // makes both explicit release and transport disconnect release the
            // same permits with no polling or daemon-global lease registry.
            let resident_capacity = match state
                .compile_service
                .acquire_resident_capacity(permits)
                .await
            {
                Ok(capacity) => capacity,
                Err(error) => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Error(format!(
                            "acquire resident compile capacity: {error}"
                        )),
                    )
                    .await;
                    return Ok(());
                }
            };

            if let Ok(true) =
                serve_resident_capacity_lease(&mut stream, permits, resident_capacity).await
            {
                state.request_count.fetch_add(1, Ordering::Relaxed);
                state.touch_activity();
            }
        }
        Request::ReleaseResidentCapacity => {
            let _ = write_frame_async(
                &mut stream,
                &Response::Error(
                    "ReleaseResidentCapacity requires an active lease on this connection"
                        .to_string(),
                ),
            )
            .await;
        }
        Request::RecordTargetTouch { path, unix_seconds } => {
            // soldr#2558: acknowledge RECEIPT before processing. On
            // macOS/BSD a connection the client closes before this server
            // accepts it is discarded together with its buffered frame, so
            // pure write-then-close fire-and-forget lost every touch that
            // raced the accept loop. The client now holds the connection
            // until this ack (bounded); the ack races nothing because it is
            // sent before the store write begins. A client that already
            // closed makes this write fail, which is fine — its frame was
            // received, which is all the ack exists to prove.
            let _ = write_frame_async(&mut stream, &Response::Ack).await;
            // Fire-and-forget for the WRITE half: errors are silent by
            // design and the client never learns the outcome.
            //
            // soldr#2224: on the blocking pool. This request arrives once
            // per rustc invocation, and a contended open waits seconds —
            // exactly the stall that must not land on a tokio worker.
            //
            // The open is retried briefly: a concurrent writer holding the
            // store used to make a single silent attempt drop the touch
            // permanently. Waiting out a transient holder keeps the write;
            // a wedged holder still ends the attempt silently at the
            // deadline, unchanged semantics.
            let db_path = state.db_path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match TargetRegistry::open(&db_path) {
                        Ok(registry) => {
                            // Silent to the CLIENT (fire-and-forget), but a
                            // failed write is worth one daemon-stderr line:
                            // an every-time failure here looked like a plain
                            // missing row and took several blind rounds to
                            // localize on the darwin target-run lanes.
                            if let Err(error) =
                                registry.upsert_with_time(Path::new(&path), unix_seconds)
                            {
                                eprintln!(
                                    "soldr-daemon: target-touch upsert failed for {path}: {error}"
                                );
                            }
                            return;
                        }
                        Err(error) => {
                            if std::time::Instant::now() >= deadline {
                                eprintln!(
                                    "soldr-daemon: target-touch dropped; could not open {} after retries: {error}",
                                    db_path.display()
                                );
                                return;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })
            .await;
        }
        Request::Status => {
            let info = state.status();
            let _ = write_frame_async(&mut stream, &Response::Status(info)).await;
        }
        Request::Shutdown => {
            peer.record_shutdown_requested(&state.paths, state.daemon_identity.started_at_unix_ms);
            let _ = write_frame_async(
                &mut stream,
                &Response::ShuttingDown(ShutdownAck {
                    pid: std::process::id(),
                    generation: state.daemon_identity.started_at_unix_ms,
                }),
            )
            .await;
            state.shutdown.request();
        }
        Request::FlushCaches => {
            // Issue #1286 (F1): checkpoint the embedded zccache state
            // (artifact index, depgraph snapshot, metadata cache) to
            // disk without shutting down. `soldr save` / `soldr cache
            // flush` call this before archiving — otherwise the state
            // is memory-only until a graceful daemon exit and archives
            // taken from a live daemon restore with zero rustc hits.
            let response = match state.event_batcher.flush().await {
                Err(err) => Response::Error(format!("event persistence flush failed: {err}")),
                Ok(()) => match state.compile_service.flush().await {
                    Ok(report) => Response::CacheFlushed(report),
                    Err(err) => Response::Error(format!("embedded zccache flush failed: {err}")),
                },
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::CompileStats => {
            // soldr#1368: return the embedded zccache service's cumulative
            // compile counters so `soldr session start/end` can diff two
            // snapshots into per-session hit/miss stats.
            let response = match state.compile_service.stats().await {
                Ok(info) => Response::CompileStats(info),
                Err(err) => Response::Error(format!("embedded zccache stats failed: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionStart {
            session_id,
            repo_root,
            started_at_ms,
        } => {
            // The cargo front door owns an OS-held BuildActivityLease before
            // sending this request and retains it through history
            // publication. Do not mirror that lease in the daemon: this
            // request has no durable connection, so a crashed client could
            // otherwise block maintenance for the daemon's whole lifetime.
            let response = async {
                // soldr#2224: read + merge + write under ONE handle, on the
                // blocking pool. Two path-taking calls here were two
                // exclusive-lock acquisitions on a tokio worker thread.
                let repo_root = repo_root.clone();
                db_async::with_handle(&state.db_path, move |db| {
                    let existing = db::get_build_in(db, session_id)?;
                    let record =
                        merge_build_session_start(existing, session_id, repo_root, started_at_ms);
                    db::upsert_build_in(db, &record)
                })
                .await
                .map_err(|err| format!("persist build session start: {err}"))?;
                // L4 (issue soldr#980): route the SessionStart event through
                // the batcher so we don't compete with the build's first
                // compile-event burst for the redb writer.
                state
                    .event_batcher
                    .record(db::Event {
                        ts_ms: started_at_ms,
                        session_id: Some(session_id),
                        kind: db::EventKind::SessionStart,
                        crate_name: None,
                        duration_us: None,
                        target_dir: None,
                        exit_code: None,
                    })
                    .await
                    .map_err(|err| format!("queue session start event: {err}"))?;
                state
                    .event_batcher
                    .flush()
                    .await
                    .map_err(|err| format!("flush session start event: {err}"))?;
                Ok::<(), String>(())
            }
            .await;
            let response = match response {
                // soldr#2023: the ack reports the limit this daemon is
                // running with, so the front door can warn on drift.
                Ok(()) => crate::compile_limit::build_session_started(
                    state.compile_service.applied_jobs(),
                ),
                Err(err) => Response::Error(err),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildSessionEnd {
            session_id,
            exit_code,
            ended_at_ms,
        } => {
            let response = match finalize_build_session(
                &state.db_path,
                &state.event_batcher,
                session_id,
                exit_code,
                ended_at_ms,
            )
            .await
            {
                Ok(()) => Response::Ack,
                Err(err) => Response::Error(err),
            };
            // soldr#1536: acknowledge the finalization. When the wrapper
            // sees this Ack, the BuildRecord aggregate is persisted and
            // every staged session event is durable in redb, so it can
            // skip its own full-table aggregate re-scan.
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::BuildLogInputs { session_id } => {
            // soldr#1814 slice 2a: the daemon owns these tables, so it answers
            // both reads in one round trip. The CLI previously opened
            // state.sqlite3 twice per build to get them.
            // soldr#2224: one handle for both reads, on the blocking pool.
            let inputs = db_async::with_handle(&state.db_path, move |db| {
                let events = db::list_events_for_session_in(db, session_id)?;
                // A missing row is normal, not an error — the log renders
                // without it. Only a genuine read failure is reported.
                let record = db::get_build_in(db, session_id).ok().flatten();
                Ok((events, record))
            })
            .await;
            let response = match inputs {
                Ok((events, record)) => Response::BuildLogInputs {
                    events,
                    record: record.map(Box::new),
                },
                Err(err) => Response::Error(format!("build log inputs: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::AttachBuildLogHistory(update) => {
            // soldr#1814 slice 2d. The whole get/mutate/upsert runs here, under
            // the daemon's own ownership of the table, so two processes cannot
            // interleave a read and a write and lose each other's fields.
            //
            // The merge deliberately reproduces the semantics the CLI-side code
            // had: `ended_at_ms` / `exit_code` keep an already-recorded value
            // rather than being overwritten (first writer wins), and the
            // crate-count aggregate is only recomputed when the client says
            // `BuildSessionEnd` did not already finalize it (soldr#1536).
            let response = db_async::with_handle(&state.db_path, move |db| {
                Ok(attach_build_log_history(db, &update))
            })
            .await
            .unwrap_or_else(|err| Response::Error(format!("read build history: {err}")));
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ShouldWarnCargoDebugDefault { repo_root } => {
            // soldr#1814 slice 2c: the daemon owns state_db's tables, so it
            // performs this read-modify-write (record the repo, prune expired
            // rows) instead of every front-door invocation opening the file.
            let db_path = crate::cache_lib::state_db_path(&state.paths);
            // soldr#2224: `StateDb::open` is the same exclusive `state.sqlite3`
            // open as everything else here, so it belongs on the blocking
            // pool rather than a tokio worker.
            let emit = tokio::task::spawn_blocking(move || {
                crate::cache_lib::state_db::StateDb::open(&db_path)
                    .and_then(|db| {
                        db.should_emit_cargo_debug_default_warning(std::path::Path::new(&repo_root))
                    })
                    // Fail open, matching the pre-#1814 caller: a state-DB
                    // problem must not silently suppress a warning the user
                    // should see.
                    .unwrap_or(true)
            })
            .await
            .unwrap_or(true);
            let _ = write_frame_async(&mut stream, &Response::CargoDebugWarning { emit }).await;
        }
        Request::ListBuilds { limit, since_ms } => {
            let response = match db_async::list_builds(&state.db_path, limit, since_ms).await {
                Ok(rows) => Response::Builds(rows),
                Err(err) => Response::Error(format!("list builds: {err}")),
            };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::ListSlowBuilds {
            threshold_ms,
            limit,
        } => {
            let response =
                match db_async::list_slow_builds(&state.db_path, threshold_ms, limit).await {
                    Ok(rows) => Response::Builds(rows),
                    Err(err) => Response::Error(format!("list slow builds: {err}")),
                };
            let _ = write_frame_async(&mut stream, &response).await;
        }
        Request::CookLookup {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            origin_url_normalized,
            branch_lineage,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            // The two cook_index calls below each serialize their own
            // redb open via `redb_lock::state_db_open_lock` (#608), so
            // no outer mutex is needed. The window between the two
            // opens admits a concurrent writer; the worst case is a
            // stale `previous_origin_recipe_hashes` drift list, which
            // is purely advisory.
            let reply = {
                match cook_index::lookup(&state.db_path, &key) {
                    Ok(Some(entry)) => {
                        state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                        let path = cook_artifact_path(&state.paths, &entry.sha256)
                            .display()
                            .to_string();
                        Response::CookHit {
                            sha256: entry.sha256,
                            path,
                            size_bytes: entry.size_bytes,
                            origin_url_normalized: entry.origin_url_normalized,
                            matched_recipe_hash: Some(recipe_hash),
                            exact_recipe_match: true,
                            branch_name: entry.branch_name,
                            compile_duration_ms: entry.compile_duration_ms,
                            save_elapsed_ms: entry.save_elapsed_ms,
                        }
                    }
                    Ok(None) => {
                        match cook_index::lookup_origin_fallback(
                            &state.db_path,
                            &key,
                            origin_url_normalized.as_deref(),
                            &branch_lineage,
                        ) {
                            Ok(Some((matched_key, entry))) => {
                                state.cook_hits_this_session.fetch_add(1, Ordering::Relaxed);
                                let path = cook_artifact_path(&state.paths, &entry.sha256)
                                    .display()
                                    .to_string();
                                Response::CookHit {
                                    sha256: entry.sha256,
                                    path,
                                    size_bytes: entry.size_bytes,
                                    origin_url_normalized: entry.origin_url_normalized,
                                    matched_recipe_hash: Some(matched_key.recipe_hash),
                                    exact_recipe_match: false,
                                    branch_name: entry.branch_name,
                                    compile_duration_ms: entry.compile_duration_ms,
                                    save_elapsed_ms: entry.save_elapsed_ms,
                                }
                            }
                            Ok(None) => {
                                let previous = cook_index::drift_recipe_hashes(
                                    &state.db_path,
                                    &key,
                                    origin_url_normalized.as_deref(),
                                    COOK_DRIFT_LIMIT,
                                )
                                .unwrap_or_default();
                                Response::CookMiss {
                                    previous_origin_recipe_hashes: previous,
                                }
                            }
                            Err(e) => {
                                Response::Error(format!("cook_index fallback lookup failed: {e}"))
                            }
                        }
                    }
                    Err(e) => Response::Error(format!("cook_index lookup failed: {e}")),
                }
            };
            let _ = write_frame_async(&mut stream, &reply).await;
        }
        Request::CookRecord {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            sha256,
            size_bytes,
            origin_url_normalized,
            branch_name,
            cook_cmd_summary,
            compile_duration_ms,
            save_elapsed_ms,
        } => {
            let key = CookKey {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
            };
            let now_ms = current_unix_ms();
            let entry = CookEntry {
                sha256,
                size_bytes,
                created_unix_ms: now_ms,
                last_used_unix_ms: now_ms,
                origin_url_normalized,
                cook_cmd_summary,
                branch_name,
                compile_duration_ms,
                save_elapsed_ms,
            };
            let result = cook_index::upsert(&state.db_path, &key, &entry);
            match result {
                Ok(()) => {
                    let _ = write_frame_async(&mut stream, &Response::Ack).await;
                }
                Err(e) => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Error(format!("cook_index upsert failed: {e}")),
                    )
                    .await;
                }
            }
        }
        Request::CookTouch { sha256 } => {
            // Fire-and-forget bump of last_used_unix_ms. Silent on
            // failure — the caller already moved on.
            let _ = cook_index::touch(&state.db_path, &sha256, current_unix_ms());
        }
        Request::Compile(req) => {
            // Issue #977 / #980 L1: dispatch the rustc compile through
            // the daemon's embedded zccache service. There is no
            // fallback path — embedded is mandatory.
            //
            // #983 Phase 5b: stream the captured stdout/stderr back to
            // the wrapper as a sequence of chunk frames followed by
            // exactly one CompileDone frame. `dispatch_compile_streaming`
            // owns the writer for the duration of the call.
            //
            // Cancellation: `dispatch_compile_streaming` watches the IPC
            // read side for disconnect concurrently with the in-flight
            // compile. If the client (rustc-wrapper) terminates — Ctrl-C
            // on the parent cargo, a hung wrapper killed by the user —
            // the daemon drops the compile future immediately so rustc
            // is cleaned up by its `kill_on_drop` chain rather than
            // grinding to completion on output no one will read.
            // Admission applies on every transport (soldr#1853). This was
            // `#[cfg(windows)]`-only, which left the AF_UNIX listener with no
            // bound at all: under `cargo -j N` it admitted every wrapper at
            // once and shed the excess by resetting sockets, which reached the
            // client as ECONNRESET and failed the build. Windows passed
            // precisely because it had this cap. The policy itself was already
            // written to be portable — see
            // `windows_burst_policy_keeps_four_pool_sizes_fifo_and_recovers`,
            // which validates it on Linux — only its application was gated.
            let _admission = match state.compile_admission.try_admit() {
                Some(permit) => permit,
                None => {
                    let _ = write_frame_async(
                        &mut stream,
                        &Response::Backpressure {
                            retry_after_ms: IPC_BACKPRESSURE_RETRY_AFTER_MS,
                        },
                    )
                    .await;
                    return Ok(());
                }
            };
            state
                .compile_admission
                .record_busy_retries(req.ipc_busy_retries);
            if let Err(err) = dispatch_compile_streaming(&state, req, &mut stream).await {
                tracing::warn!("soldr-daemon: streaming compile dispatch failed: {err}");
            }
        }
    }
    Ok(())
}

/// Hold an opaque embedded-service capacity guard for the lifetime of one
/// control connection. `Ok(true)` means the peer explicitly released it;
/// `Ok(false)` means it sent a different frame. Transport errors include EOF.
async fn serve_resident_capacity_lease<S, G>(
    stream: &mut S,
    permits: u32,
    resident_capacity: G,
) -> std::io::Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    write_frame_async(
        &mut *stream,
        &Response::ResidentCapacityAcquired { permits },
    )
    .await?;

    match crate::daemon::ipc::read_frame_async::<_, Request>(&mut *stream).await? {
        Request::ReleaseResidentCapacity => {
            // Ack means release is complete, so drop before writing it rather
            // than relying on scope exit after the response is in flight.
            drop(resident_capacity);
            write_frame_async(&mut *stream, &Response::Ack).await?;
            Ok(true)
        }
        other => {
            write_frame_async(
                &mut *stream,
                &Response::Error(format!(
                    "expected ReleaseResidentCapacity, received {other:?}"
                )),
            )
            .await?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod resident_capacity_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn explicit_release_drops_capacity_before_ack() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let dropped = Arc::new(AtomicBool::new(false));
        let server_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            serve_resident_capacity_lease(&mut server, 2, DropProbe(server_dropped)).await
        });

        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("acquired response"),
            Response::ResidentCapacityAcquired { permits: 2 }
        ));
        assert!(!dropped.load(Ordering::SeqCst));
        crate::daemon::ipc::write_frame_async(
            &mut client,
            &Request::ReleaseResidentCapacity,
        )
        .await
        .expect("release request");
        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("release ack"),
            Response::Ack
        ));
        assert!(dropped.load(Ordering::SeqCst));
        assert!(task.await.expect("server task").expect("serve lease"));
    }

    #[tokio::test]
    async fn disconnect_drops_capacity_without_release() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let dropped = Arc::new(AtomicBool::new(false));
        let server_dropped = dropped.clone();
        let task = tokio::spawn(async move {
            serve_resident_capacity_lease(&mut server, 1, DropProbe(server_dropped)).await
        });

        assert!(matches!(
            crate::daemon::ipc::read_frame_async::<_, Response>(&mut client)
                .await
                .expect("acquired response"),
            Response::ResidentCapacityAcquired { permits: 1 }
        ));
        drop(client);
        assert!(task.await.expect("server task").is_err());
        assert!(dropped.load(Ordering::SeqCst));
    }
}

// Daemon-side streaming compile dispatcher (issue #983 Phase 5b /
// soldr#981).
//
// Calls `SoldrZccacheService::compile`, then splits the captured
// stdout/stderr `Vec<u8>` into `CHUNK_BYTES`-sized frames before
// writing them to the connection. The terminal `CompileDone` frame
// carries the exit code, cache outcome, and (today empty) compile id.
//
// **Wire contract locked in `tests/phase5_contract.rs`** — that
// regression test asserts the chunked `Response::CompileStdoutChunk`
// / `CompileStderrChunk` / `CompileDone` variants round-trip
// byte-for-byte over the prost codec. If anyone re-introduces the
// single-frame `Response::Compile(body)` shape from the v6-era
// fork-zccache.exe path, that test fails with a directive message
// pointing at #981.
//
// **Phase 5b1 caveat:** the underlying `compile_service.compile`
// still returns a fully buffered `CompileResponseBody`, so the
// daemon briefly holds the entire rustc output in memory before
// chunking it out. The on-wire saving (smaller per-frame prost
// encode + zero wrapper-side accumulation) is the immediate win;
// **Phase 5b2** lifts the daemon-side buffering by switching to the
// already-published `compile_service.compile_streaming(req,
// |chunk| …)` API, whose producer side will start emitting chunks
// incrementally once `zccache#937` (cross-cutting daemon-pipeline
// streaming) lands upstream in zccache. The consumer surface is
// already in place: this function chunks output identically to what
// `compile_streaming` emits today, so the migration is mechanical
// and the wire bytes don't change.
