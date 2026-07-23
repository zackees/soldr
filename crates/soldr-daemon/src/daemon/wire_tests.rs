//! Round-trip unit tests for the daemon wire encode/decode layer.
//! Extracted from `wire.rs` (soldr#1368) via `#[path = "wire_tests.rs"]`.

use super::*;
use crate::daemon::protocol::{
    BuildCacheSummary, BuildLogPaths, BuildMissReason, CacheFlushInfo, CacheFlushStepInfo,
    Response, ShutdownAck, StatusInfo,
};

crate::timed_test!(record_target_touch_round_trips, {
    let req = Request::RecordTargetTouch {
        path: "/tmp/target".into(),
        unix_seconds: 1_700_000_000,
    };
    let bytes = encode_request(&req);
    let decoded = decode_request(&bytes).expect("decode");
    assert!(matches!(
        decoded,
        Request::RecordTargetTouch { ref path, unix_seconds }
            if path == "/tmp/target" && unix_seconds == 1_700_000_000
    ));
});

crate::timed_test!(status_request_round_trips, {
    let bytes = encode_request(&Request::Status);
    assert!(matches!(
        decode_request(&bytes).expect("decode"),
        Request::Status
    ));
});

crate::timed_test!(flush_caches_request_round_trips, {
    let bytes = encode_request(&Request::FlushCaches);
    assert!(matches!(
        decode_request(&bytes).expect("decode"),
        Request::FlushCaches
    ));
});

crate::timed_test!(cache_flush_response_preserves_incomplete_step_details, {
    let info = CacheFlushInfo {
        complete: false,
        pending_writes_drained: true,
        index_writer_drained: true,
        steps: vec![
            CacheFlushStepInfo {
                step: "artifact_store".into(),
                status: "completed".into(),
                error: None,
            },
            CacheFlushStepInfo {
                step: "depgraph".into(),
                status: "failed".into(),
                error: Some("disk full".into()),
            },
        ],
        artifact_entries: 41,
        metadata_entries: 73,
    };
    let decoded =
        decode_response(&encode_response(&Response::CacheFlushed(info.clone()))).expect("decode");
    match decoded {
        Response::CacheFlushed(decoded) => assert_eq!(decoded, info),
        other => panic!("expected CacheFlushed, got {other:?}"),
    }
});

crate::timed_test!(compile_stats_verb_round_trips, {
    use crate::daemon::protocol::{CompileStatsInfo, StagedProfileInfo};
    assert!(matches!(
        decode_request(&encode_request(&Request::CompileStats)).expect("decode"),
        Request::CompileStats
    ));
    let info = CompileStatsInfo {
        total_compilations: 100,
        cache_hits: 73,
        cache_misses: 21,
        non_cacheable: 4,
        compile_errors: 2,
        time_saved_ms: 123_456,
        staged_profile: Some(StagedProfileInfo {
            counters: [("published".to_string(), 7)].into(),
            ..Default::default()
        }),
    };
    match decode_response(&encode_response(&Response::CompileStats(info.clone()))).expect("decode")
    {
        Response::CompileStats(decoded) => assert_eq!(decoded, info),
        other => panic!("expected CompileStats, got {other:?}"),
    }
});

crate::timed_test!(cook_lookup_round_trips_with_all_fields, {
    let req = Request::CookLookup {
        recipe_hash: [0x42; 32],
        target_triple: "x86_64-pc-windows-msvc".into(),
        profile: "release".into(),
        channel: "1.94.1".into(),
        rustc_version: "rustc 1.94.1".into(),
        origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        branch_lineage: vec!["feature/cook".into(), "main".into()],
    };
    let bytes = encode_request(&req);
    let decoded = decode_request(&bytes).expect("decode");
    match decoded {
        Request::CookLookup {
            recipe_hash,
            target_triple,
            profile,
            channel,
            rustc_version,
            origin_url_normalized,
            branch_lineage,
        } => {
            assert_eq!(recipe_hash, [0x42; 32]);
            assert_eq!(target_triple, "x86_64-pc-windows-msvc");
            assert_eq!(profile, "release");
            assert_eq!(channel, "1.94.1");
            assert_eq!(rustc_version, "rustc 1.94.1");
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr")
            );
            assert_eq!(branch_lineage, vec!["feature/cook", "main"]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(cook_record_round_trips_with_sha_validation, {
    let req = Request::CookRecord {
        recipe_hash: [0x11; 32],
        target_triple: "x86_64-unknown-linux-gnu".into(),
        profile: "release".into(),
        channel: "1.94.1".into(),
        rustc_version: "rustc 1.94.1".into(),
        sha256: [0xAA; 32],
        size_bytes: 4_096,
        origin_url_normalized: None,
        branch_name: Some("main".into()),
        cook_cmd_summary: "cook --release".into(),
    };
    let bytes = encode_request(&req);
    let Request::CookRecord {
        recipe_hash,
        sha256,
        size_bytes,
        branch_name,
        ..
    } = decode_request(&bytes).expect("decode")
    else {
        panic!("expected CookRecord");
    };
    assert_eq!(recipe_hash, [0x11; 32]);
    assert_eq!(sha256, [0xAA; 32]);
    assert_eq!(size_bytes, 4_096);
    assert_eq!(branch_name.as_deref(), Some("main"));
});

crate::timed_test!(status_response_round_trips_with_cook_stats, {
    let info = StatusInfo {
        version: 7,
        pid: 4242,
        generation: 1_700_000_000_123,
        uptime_secs: 60,
        request_count: 17,
        cook_stats: Some(CookStats {
            entries: 3,
            total_bytes: 9_999,
            hits_this_session: 1,
        }),
        compile_backend: "embedded".to_string(),
        ipc_burst_stats: crate::daemon::protocol::IpcBurstStats {
            accepted: 16,
            queued: 7,
            backpressured: 3,
            busy_retries: 2,
            queue_high_water: 16,
        },
    };
    let resp = Response::Status(info.clone());
    let bytes = encode_response(&resp);
    let decoded = decode_response(&bytes).expect("decode");
    match decoded {
        Response::Status(decoded_info) => assert_eq!(decoded_info, info),
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(shutdown_ack_round_trips_responder_generation, {
    let response = Response::ShuttingDown(ShutdownAck {
        pid: 4242,
        generation: 1_700_000_000_123,
    });
    let bytes = encode_response(&response);
    assert!(matches!(
        decode_response(&bytes).expect("decode"),
        Response::ShuttingDown(ShutdownAck {
            pid: 4242,
            generation: 1_700_000_000_123,
        })
    ));
});

crate::timed_test!(legacy_empty_shutdown_ack_decodes_with_zero_identity, {
    // v17 encoded WireResponse.shutting_down as an empty nested message.
    // v18 must continue to decode that shape so the client can pair it with
    // the immediately preceding v17 Status response.
    let legacy_bytes = [0x12, 0x00];
    assert!(matches!(
        decode_response(&legacy_bytes).expect("decode legacy shutdown ack"),
        Response::ShuttingDown(ShutdownAck {
            pid: 0,
            generation: 0,
        })
    ));
});

crate::timed_test!(cook_hit_response_round_trips, {
    let resp = Response::CookHit {
        sha256: [0xCC; 32],
        path: "/home/runner/.soldr/cache/cook/abcd.tar.zst".into(),
        size_bytes: 4_096,
        origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        matched_recipe_hash: Some([0x11; 32]),
        exact_recipe_match: false,
        branch_name: Some("main".into()),
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CookHit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
        } => {
            assert_eq!(sha256, [0xCC; 32]);
            assert!(path.ends_with(".tar.zst"));
            assert_eq!(size_bytes, 4_096);
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr")
            );
            assert_eq!(matched_recipe_hash, Some([0x11; 32]));
            assert!(!exact_recipe_match);
            assert_eq!(branch_name.as_deref(), Some("main"));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(cook_miss_response_round_trips_with_multiple_hashes, {
    let resp = Response::CookMiss {
        previous_origin_recipe_hashes: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CookMiss {
            previous_origin_recipe_hashes,
        } => {
            assert_eq!(previous_origin_recipe_hashes.len(), 3);
            assert_eq!(previous_origin_recipe_hashes[0], [1u8; 32]);
            assert_eq!(previous_origin_recipe_hashes[1], [2u8; 32]);
            assert_eq!(previous_origin_recipe_hashes[2], [3u8; 32]);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(backpressure_response_round_trips_with_retry_delay, {
    let bytes = encode_response(&Response::Backpressure { retry_after_ms: 25 });
    assert!(matches!(
        decode_response(&bytes).expect("decode"),
        Response::Backpressure { retry_after_ms: 25 }
    ));
});

crate::timed_test!(builds_response_round_trips_with_all_optional_fields, {
    let record = BuildRecord {
        session_id: 42,
        repo_root: "/r".into(),
        started_at_ms: 100,
        ended_at_ms: Some(500),
        exit_code: Some(0),
        total_wall_ms: Some(400),
        crate_count: 7,
        slowest_crate_us: Some(123_456),
        slowest_crate_name: Some("zccache".into()),
        cache_summary: Some(BuildCacheSummary {
            hits: 13,
            misses: 5,
            non_cacheable: 2,
            errors: 1,
            compilations: 21,
            time_saved_ms: 900,
        }),
        log_paths: Some(BuildLogPaths {
            zccache_session_id: Some("session-abc".into()),
            cache_dir: Some("/cache/zccache".into()),
            session_log_path: Some("/cache/zccache/logs/last-session.log".into()),
            journal_path: Some("/cache/zccache/logs/last-session.jsonl".into()),
            session_stats_path: Some("/cache/zccache/logs/last-session-stats.json".into()),
            compile_journal_path: Some("/cache/zccache/logs/compile_journal.jsonl".into()),
            archived_session_log_path: Some("/cache/zccache/history/42/last-session.log".into()),
            archived_journal_path: Some("/cache/zccache/history/42/last-session.jsonl".into()),
            archived_session_stats_path: Some(
                "/cache/zccache/history/42/last-session-stats.json".into(),
            ),
            archived_compile_journal_path: Some(
                "/cache/zccache/history/42/compile_journal.jsonl".into(),
            ),
            private_daemon_name: Some("soldr-dev-demo".into()),
        }),
        miss_reasons: vec![BuildMissReason {
            reason: "key_mismatch".into(),
            count: 5,
        }],
    };
    let resp = Response::Builds(vec![record.clone()]);
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::Builds(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0], record);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(invalid_sha_length_is_a_decode_error, {
    // Build a WireCookTouch by hand with a bogus 16-byte sha.
    let bad = proto::WireRequest {
        kind: Some(proto::WireRequestKind::CookTouch(proto::WireCookTouch {
            sha256: vec![0u8; 16],
        })),
    };
    let mut bytes = Vec::new();
    prost::Message::encode(&bad, &mut bytes).expect("encode");
    let err = decode_request(&bytes).expect_err("must error");
    assert!(matches!(err, WireDecodeError::InvalidShaLength(16)));
});

crate::timed_test!(empty_request_oneof_is_a_decode_error, {
    let bytes = Vec::new(); // empty WireRequest with no oneof
    let err = decode_request(&bytes).expect_err("must error");
    assert!(matches!(err, WireDecodeError::EmptyOneof("Request")));
});

crate::timed_test!(event_kind_round_trips_through_u32, {
    for kind in [
        EventKind::SessionStart,
        EventKind::SessionEnd,
        EventKind::CompileStart,
        EventKind::CompileEnd,
    ] {
        let n = event_kind_to_u32(&kind);
        assert_eq!(u32_to_event_kind(n).unwrap(), kind);
    }
    assert!(matches!(
        u32_to_event_kind(99).unwrap_err(),
        WireDecodeError::UnknownEventKind(99)
    ));
});

crate::timed_test!(prost_tagged_bytes_prepends_the_tag, {
    let payload = proto::WireUnit {};
    let bytes = prost_tagged_bytes(&payload);
    assert_eq!(bytes.first().copied(), Some(REDB_TAG_PROST));
});

crate::timed_test!(compile_request_round_trips_with_env_and_cwd, {
    let req = Request::Compile(crate::daemon::protocol::CompileRequest {
        args: vec![
            "/usr/bin/rustc".into(),
            "--crate-name=foo".into(),
            "--edition=2021".into(),
        ],
        cwd: "/home/runner/work/soldr".into(),
        env: vec![
            ("CARGO_PKG_NAME".into(), "soldr".into()),
            ("OPT_LEVEL".into(), "3".into()),
        ],
        stdin: vec![1, 2, 3, 4],
        lifecycle: Some(crate::daemon::protocol::CompileLifecycle {
            session_id: 42,
            crate_name: "foo".into(),
            target_dir: "/home/runner/work/soldr/target".into(),
            started_at_ms: 1_700_000_000_123,
        }),
        ipc_busy_retries: 3,
    });
    let bytes = encode_request(&req);
    match decode_request(&bytes).expect("decode") {
        Request::Compile(decoded) => {
            assert_eq!(decoded.args.len(), 3);
            assert_eq!(decoded.cwd, "/home/runner/work/soldr");
            assert_eq!(decoded.env.len(), 2);
            assert_eq!(decoded.env[0], ("CARGO_PKG_NAME".into(), "soldr".into()));
            assert_eq!(decoded.stdin, vec![1, 2, 3, 4]);
            assert_eq!(decoded.ipc_busy_retries, 3);
            let lifecycle = decoded.lifecycle.expect("compile lifecycle metadata");
            assert_eq!(lifecycle.session_id, 42);
            assert_eq!(lifecycle.crate_name, "foo");
            assert_eq!(lifecycle.target_dir, "/home/runner/work/soldr/target");
            assert_eq!(lifecycle.started_at_ms, 1_700_000_000_123);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(compile_response_round_trips, {
    let body = crate::daemon::protocol::CompileResponseBody {
        exit_code: 0,
        stdout: b"compiling\n".to_vec(),
        stderr: b"warning: unused import\n".to_vec(),
        cached: true,
        cache_outcome: 1,
    };
    let resp = Response::Compile(body.clone());
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::Compile(decoded) => {
            assert_eq!(decoded.exit_code, 0);
            assert_eq!(decoded.stdout, b"compiling\n");
            assert_eq!(decoded.stderr, b"warning: unused import\n");
            assert!(decoded.cached);
            assert_eq!(decoded.cache_outcome, 1);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(compile_stdout_chunk_round_trips, {
    // #983 Phase 5b — streaming chunk variant. Exercises the
    // happy path including zero-byte chunks (which the daemon
    // never emits, but the decode side must still accept).
    let payload = b"rustc: compiling foo v0.1.0\n".to_vec();
    let resp = Response::CompileStdoutChunk(payload.clone());
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CompileStdoutChunk(decoded) => {
            assert_eq!(decoded, payload);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    let empty = Response::CompileStdoutChunk(Vec::new());
    let bytes = encode_response(&empty);
    match decode_response(&bytes).expect("decode") {
        Response::CompileStdoutChunk(decoded) => assert!(decoded.is_empty()),
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(compile_stderr_chunk_round_trips, {
    // #983 Phase 5b — the stderr counterpart. Same shape, separate
    // discriminant so the wrapper-side reader can fan out to the
    // correct sink without inspecting the payload.
    let payload = b"warning: unused import `foo`\n".to_vec();
    let resp = Response::CompileStderrChunk(payload.clone());
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CompileStderrChunk(decoded) => {
            assert_eq!(decoded, payload);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});

crate::timed_test!(compile_done_round_trips_with_all_fields, {
    // #983 Phase 5b — terminal frame in the streaming reply. All
    // four metadata fields are load-bearing for the wrapper's
    // exit-code + cache-outcome reporting.
    let resp = Response::CompileDone {
        exit_code: 0,
        cached: true,
        cache_outcome: 1,
        compile_id: "abc123-def456".into(),
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CompileDone {
            exit_code,
            cached,
            cache_outcome,
            compile_id,
        } => {
            assert_eq!(exit_code, 0);
            assert!(cached);
            assert_eq!(cache_outcome, 1);
            assert_eq!(compile_id, "abc123-def456");
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    // Non-zero exit_code + empty compile_id (the daemon emits an
    // empty string when zccache does not surface an audit id).
    let resp = Response::CompileDone {
        exit_code: 101,
        cached: false,
        cache_outcome: 2,
        compile_id: String::new(),
    };
    let bytes = encode_response(&resp);
    match decode_response(&bytes).expect("decode") {
        Response::CompileDone {
            exit_code,
            cached,
            cache_outcome,
            compile_id,
        } => {
            assert_eq!(exit_code, 101);
            assert!(!cached);
            assert_eq!(cache_outcome, 2);
            assert!(compile_id.is_empty());
        }
        other => panic!("unexpected variant: {other:?}"),
    }
});
