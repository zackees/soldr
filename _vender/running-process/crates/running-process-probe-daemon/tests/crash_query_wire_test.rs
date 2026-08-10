//! Crash history over the real request path (S12 / #641).
//!
//! `crash_query`'s unit tests drive SQL against a seeded database. These drive
//! the surface an operator actually reaches: a `probe_diag.v1` body, through
//! `ProbeOps::dispatch`, back out as a wire reply. That path is where the
//! redaction contract has to hold, because it is the only one that leaves the
//! process.

use std::num::NonZeroU32;
use std::sync::Arc;

use running_process::broker::server::{PeerCredentialPolicy, PeerIdentity};
use running_process_probe::probe_diag::v1 as wire;
use running_process_probe::probe_diag::v1::probe_envelope::Body;
use running_process_probe_daemon::crash_query::{CrashFilter, MAX_CRASH_LIMIT};
use running_process_probe_daemon::crash_store::CrashStore;
use running_process_probe_daemon::probe_ops::{
    IdentityVerdict, ProbeOps, ProbeReply, ProbeRequest,
};
use running_process_probe_daemon::registry::Registry;
use running_process_probe_daemon::wire_convert::{envelope_from_reply, request_from_envelope};
use rusqlite::params;
use tempfile::TempDir;

const OWNER: &str = "crash-owner";

/// A secret planted in every seeded row's inline report.
///
/// If it ever appears on the wire, the redaction contract is broken, and the
/// test says so by name rather than by a count that could drift.
const PLANTED_SECRET: &str = "AWS_SECRET_ACCESS_KEY=hunter2";

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: OWNER.to_string(),
    }
}

struct Harness {
    _dir: TempDir,
    ops: ProbeOps,
}

impl Harness {
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = CrashStore::open(
            &dir.path().join("crashes.db"),
            &dir.path().join("artifacts"),
        )
        .expect("open crash store");
        let ops = ProbeOps::new(
            Arc::new(Registry::new(OWNER.to_string())),
            PeerCredentialPolicy::OwnerOnly {
                uid_or_sid: OWNER.to_string(),
            },
        )
        .with_crash_store(Arc::new(store));
        let harness = Self { _dir: dir, ops };
        harness.seed("clud", "SIGSEGV@parse", 1_000, 11);
        harness.seed("clud", "SIGSEGV@parse", 2_000, 12);
        harness.seed("clud-worker", "SIGSEGV@parse", 3_000, 13);
        harness.seed("clud-worker", "SIGABRT@assert", 4_000, 14);
        harness.seed("other", "SIGILL@jit", 5_000, 15);
        harness
    }

    fn seed(&self, app_class: &str, signature: &str, crashed_at_ms: i64, pid: i64) {
        let store = self.ops.crash_store().expect("crash store attached");
        let conn = store.connection_for_test().lock().expect("store lock");
        conn.execute(
            "INSERT INTO crashes (app_class, app_name, app_version, instance_name, pid,
                                  creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                                  report_json, artifact_path, artifact_bytes)
             VALUES (?1, ?1, '1.0', 'a', ?2, 7, '/work/secret-dir', ?3, ?4, 'SIGSEGV',
                     ?5, 'crash.json', 4096)",
            params![
                app_class,
                pid,
                signature,
                crashed_at_ms,
                format!(r#"{{"env":"{PLANTED_SECRET}"}}"#),
            ],
        )
        .expect("seed crash row");
    }

    /// Send a wire body through the whole request path and return the reply body.
    fn round_trip(&self, body: Body) -> Body {
        let envelope = wire::ProbeEnvelope {
            wire_version: 1,
            request_id: 7,
            deadline_unix_ms: 0,
            body: Some(body),
        };
        let request = request_from_envelope(envelope).expect("body should be served");
        let reply = self.ops.dispatch(
            request,
            &peer(),
            1,
            IdentityVerdict {
                verified: true,
                connection_alive: true,
            },
        );
        envelope_from_reply(7, &reply).body.expect("reply body")
    }

    fn crashes(&self, query: wire::CrashQuery) -> wire::CrashQueryReply {
        match self.round_trip(Body::CrashQuery(query)) {
            Body::CrashQueryReply(reply) => reply,
            other => panic!("expected CrashQueryReply, got {other:?}"),
        }
    }

    fn stats(&self, filter: wire::CrashQuery) -> wire::CrashStatsReply {
        let body = Body::CrashStatsQuery(wire::CrashStatsQuery {
            filter: Some(filter),
        });
        match self.round_trip(body) {
            Body::CrashStatsReply(reply) => reply,
            other => panic!("expected CrashStatsReply, got {other:?}"),
        }
    }
}

fn query(limit: u32) -> wire::CrashQuery {
    wire::CrashQuery {
        limit,
        ..Default::default()
    }
}

#[test]
fn a_class_filter_round_trips_over_the_wire() {
    let harness = Harness::new();
    let reply = harness.crashes(wire::CrashQuery {
        app_class: "clud".into(),
        ..query(10)
    });
    assert_eq!(reply.error, 0);
    assert_eq!(reply.records.len(), 2);
    assert!(reply.records.iter().all(|r| r.app_class == "clud"));
}

#[test]
fn a_like_filter_sweeps_the_whole_family() {
    let harness = Harness::new();
    let reply = harness.crashes(wire::CrashQuery {
        app_class_like: "clud%".into(),
        ..query(10)
    });
    assert_eq!(reply.records.len(), 4);
}

#[test]
fn stats_roll_up_by_signature_across_classes() {
    let harness = Harness::new();
    let reply = harness.stats(query(0));

    assert_eq!(reply.total, 5);
    assert_eq!(reply.distinct_classes, 3);
    assert_eq!(reply.first_unix_ms, 1_000);
    assert_eq!(reply.last_unix_ms, 5_000);

    let top = &reply.signatures[0];
    assert_eq!(top.signature, "SIGSEGV@parse");
    assert_eq!(top.count, 3);
    assert_eq!(top.app_classes, vec!["clud", "clud-worker"]);
}

#[test]
fn stats_do_not_require_a_limit() {
    // A rollup is bounded by the number of distinct signatures, not by the
    // number of crashes, so demanding a limit would only invite a caller to
    // pick one that silently drops buckets.
    let harness = Harness::new();
    assert_eq!(harness.stats(query(0)).total, 5);
}

#[test]
fn a_record_query_without_a_limit_is_not_served() {
    // The engine refuses an unbounded page, so the body never becomes a
    // request at all — the caller gets a structured refusal, not a dump of
    // the whole retained history.
    let body = Body::CrashQuery(query(0));
    let envelope = wire::ProbeEnvelope {
        wire_version: 1,
        request_id: 1,
        deadline_unix_ms: 0,
        body: Some(body),
    };
    assert!(request_from_envelope(envelope).is_none());
}

#[test]
fn an_oversized_limit_is_refused_with_a_reason() {
    let harness = Harness::new();
    let reply = harness.crashes(query(MAX_CRASH_LIMIT + 1));
    assert_ne!(reply.error, 0, "an oversized page must not be served");
}

#[test]
fn a_reply_never_carries_the_artifact_path_or_the_inline_report() {
    // The two things the redaction contract is about: the daemon-private path
    // (which discloses the owner's directory layout) and the inline crash
    // report (which holds whatever the process was doing).
    let harness = Harness::new();
    let reply = harness.crashes(query(100));
    let rendered = format!("{reply:?}");

    assert!(
        !rendered.contains(PLANTED_SECRET),
        "the inline crash report must never reach a query surface"
    );
    assert!(
        !rendered.contains("crash.json"),
        "the daemon-private artifact path must never reach a query surface"
    );

    // What a caller gets instead: an opaque id and a size, enough to decide
    // whether to fetch the bytes through the artifact endpoint.
    let record = &reply.records[0];
    assert!(record.id > 0);
    assert_eq!(record.artifact_bytes, 4096);
}

#[test]
fn a_stats_reply_carries_no_per_crash_detail() {
    let harness = Harness::new();
    let rendered = format!("{:?}", harness.stats(query(0)));
    assert!(!rendered.contains(PLANTED_SECRET));
    assert!(!rendered.contains("secret-dir"), "no cwd in an aggregate");
}

#[test]
fn a_non_owner_peer_cannot_read_crash_history() {
    let harness = Harness::new();
    let stranger = PeerIdentity {
        pid: 9999,
        uid_or_sid: "someone-else".into(),
    };
    let reply = harness.ops.dispatch(
        ProbeRequest::QueryCrashes {
            filter: Box::new(CrashFilter::default()),
            limit: NonZeroU32::new(10).unwrap(),
        },
        &stranger,
        2,
        IdentityVerdict {
            verified: true,
            connection_alive: true,
        },
    );
    assert!(
        matches!(reply, ProbeReply::Refused { .. }),
        "crash history must not be readable by another user: {reply:?}"
    );
}

#[test]
fn a_daemon_without_a_crash_store_refuses_rather_than_panics() {
    // The store is a filesystem resource that can fail to open. When it does,
    // the live surfaces keep working and crash queries say why.
    let ops = ProbeOps::new(
        Arc::new(Registry::new(OWNER.to_string())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.to_string(),
        },
    );
    let reply = ops.dispatch(
        ProbeRequest::CrashStats(Box::default()),
        &peer(),
        1,
        IdentityVerdict {
            verified: true,
            connection_alive: true,
        },
    );
    match reply {
        ProbeReply::CrashRefused { reason, stats, .. } => {
            assert!(reason.contains("crash history"));
            assert!(stats, "a refused rollup must come back on the rollup body");
        }
        other => panic!("expected a crash refusal, got {other:?}"),
    }
}

#[test]
fn a_refused_crash_query_comes_back_on_the_crash_reply_body() {
    // Not on `RegistrationStatus`. A caller matching the reply it asked for
    // must be able to read the error out of the field the schema gave it,
    // rather than treating "your limit was too large" as an unexpected
    // message it has no branch for.
    let harness = Harness::new();
    let reply = harness.crashes(query(MAX_CRASH_LIMIT + 1));
    assert_ne!(reply.error, 0);
    assert!(
        reply.detail.contains("limit"),
        "detail was {:?}",
        reply.detail
    );
    assert!(reply.records.is_empty());
}
