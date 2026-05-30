//! Wire protocol for soldr-daemon — bincode-encoded, length-prefixed
//! frames. Each frame on the wire is:
//!
//! ```text
//! [u32 LE body_len][u32 LE protocol_version][bincode body]
//! ```
//!
//! `body_len` does NOT include the 4-byte version field. The body is a
//! bincoded `Request` (client → server) or `Response` (server → client).

use serde::{Deserialize, Serialize};

/// Protocol version bump rationale:
///
/// * v1–v2: pre-PID-file lifecycle.
/// * v3: target-touch + build-session correlation + linked-zccache.
/// * v4 (this PR): adds cook-index IPC (CookLookup, CookRecord,
///   CookTouch) + `cook_stats` on `StatusInfo`. Old clients/daemons
///   on v3 cannot decode v4 frames and vice versa; the IPC layer
///   rejects mismatched versions cleanly and the wrapper hot path's
///   direct-redb fallback keeps builds from breaking during the
///   short cross-version window after a `soldr` upgrade.
pub const PROTOCOL_VERSION: u32 = 4;

/// Maximum bincode body size. 64 KiB is comfortably above the largest
/// realistic record (path strings, a few timestamps). Frames larger than
/// this are rejected on both ends to harden against a misbehaving peer.
pub const MAX_BODY_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Fire-and-forget: stamp a workspace `target/` path with `unix_seconds`
    /// in the soldr target registry. The wrapper hot path uses this.
    RecordTargetTouch { path: String, unix_seconds: i64 },
    /// Request-response: return a small structured snapshot of daemon
    /// state. Used by `soldr daemon status`.
    Status,
    /// Request-response: ask the daemon to drain and exit. Used by
    /// `soldr daemon stop` and (Phase 3) linked-zccache shutdown.
    Shutdown,
    /// Fire-and-forget: open a build session. Issued by the cargo
    /// front door immediately before spawning cargo.
    BuildSessionStart {
        session_id: u64,
        repo_root: String,
        started_at_ms: i64,
    },
    /// Fire-and-forget: finalize a build session. Issued by the cargo
    /// front door after cargo exits.
    BuildSessionEnd {
        session_id: u64,
        exit_code: i32,
        ended_at_ms: i64,
    },
    /// Fire-and-forget: record one rustc invocation inside a build
    /// session. `duration_us` is `None` on Unix where the wrapper
    /// `exec()`s into zccache and never returns (only the start event
    /// is observable from soldr's side); on Windows the wrapper waits
    /// for the spawned process and fills the duration.
    RecordCompile {
        session_id: u64,
        crate_name: String,
        target_dir: String,
        started_at_ms: i64,
        duration_us: Option<u64>,
    },
    /// Request-response: return the most recent build records, newest
    /// first, up to `limit`. Optional `since_ms` filters to records
    /// whose `started_at_ms >= since_ms`.
    ListBuilds { limit: u32, since_ms: Option<i64> },
    /// Request-response: return finished build records whose
    /// `total_wall_ms >= threshold_ms`, sorted desc by `total_wall_ms`,
    /// capped at `limit`.
    ListSlowBuilds { threshold_ms: u64, limit: u32 },
    /// Fire-and-forget: tell the daemon which zccache runtime/cache/session is
    /// linked to this soldr-daemon's session. On daemon shutdown
    /// (explicit RPC, signal, or idle timeout), the daemon issues
    /// `zccache stop` with the recorded cache dir before exiting.
    LinkZccache { link: ZccacheDaemonLink },
    /// Request-response: probe the cook-artifact index for the given
    /// `(recipe_hash, target_triple, profile, channel, rustc_version)`
    /// tuple. On hit returns [`Response::CookHit`]; on miss returns
    /// [`Response::CookMiss`] with up to N previous recipe hashes
    /// recorded under the same `(origin, triple, profile, channel,
    /// rustc)` matrix — used by the consumer (PR 3) as a "recipe
    /// drift" diagnostic when the cargo-front-door pre-flight misses.
    /// `origin_url_normalized` is a hint only and never participates
    /// in the authoritative key.
    CookLookup {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        origin_url_normalized: Option<String>,
    },
    /// Request-response: register a cook artifact written by PR 2's
    /// `soldr cook` worker at `~/.soldr/cache/cook/<sha256>.tar.zst`.
    /// Replies with [`Response::Ack`] on success.
    CookRecord {
        recipe_hash: [u8; 32],
        target_triple: String,
        profile: String,
        channel: String,
        rustc_version: String,
        sha256: [u8; 32],
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        cook_cmd_summary: String,
    },
    /// Fire-and-forget: bump the `last_used_unix_ms` field for the
    /// row whose `sha256` matches. Sent by PR 3's pre-flight after a
    /// hit so eviction can prefer rows that are actually serving
    /// traffic. Failure must never affect the caller — per-call
    /// 50 ms timeout from the client side.
    CookTouch { sha256: [u8; 32] },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Status(StatusInfo),
    ShuttingDown,
    Builds(Vec<BuildRecord>),
    Error(String),
    /// Reply to [`Request::CookLookup`] on hit. Carries the on-disk
    /// path to the `<sha256>.tar.zst` artifact, the recorded sha256
    /// (PR 3 verifies it before extraction), the byte size for
    /// hydration reporting, and the recorded origin URL hint.
    CookHit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
    },
    /// Reply to [`Request::CookLookup`] on miss. `previous_origin_recipe_hashes`
    /// is a diagnostic: at most 8 prior recipe hashes recorded under
    /// the same `(origin, triple, profile, channel, rustc)` matrix,
    /// newest-first. Used by PR 3 to print a recipe-drift line.
    CookMiss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
    /// Generic ack for fire-and-forget-style request/response calls
    /// that don't carry a payload (used by [`Request::CookRecord`]).
    Ack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusInfo {
    pub version: u32,
    pub pid: u32,
    pub uptime_secs: u64,
    pub request_count: u64,
    /// Set by `LinkZccache`; cleared on daemon shutdown.
    pub linked_zccache: Option<ZccacheDaemonLink>,
    /// Cook-index aggregate stats (issue #576). `None` means "the
    /// daemon does not expose cook stats" — old v3 daemons can never
    /// emit this (rejected at the version check), so in practice
    /// callers should treat `None` as zero. Use
    /// [`StatusInfo::cook_stats_or_zero`] to get the defaulted view.
    pub cook_stats: Option<CookStats>,
}

impl StatusInfo {
    /// Resolve `cook_stats` to a concrete value, treating `None` as
    /// the zero state. Reserved for callers that want a fully-
    /// populated rendering even when the daemon predates the
    /// cook-index feature.
    pub fn cook_stats_or_zero(&self) -> CookStats {
        self.cook_stats.clone().unwrap_or_default()
    }
}

/// Aggregate counts for the cook-artifact index. Surfaced through
/// [`Request::Status`] and rendered by `soldr daemon status` /
/// `soldr doctor`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CookStats {
    /// Number of rows currently in `cook_index_v1`.
    pub entries: u64,
    /// Sum of `size_bytes` across all rows.
    pub total_bytes: u64,
    /// Number of [`Request::CookLookup`] hits served by the running
    /// daemon since its last startup. Resets across daemon restarts.
    pub hits_this_session: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZccacheDaemonLink {
    pub binary_path: String,
    pub cache_dir: String,
    pub session_id: Option<String>,
    pub source: String,
    pub private_daemon: bool,
    pub daemon_name: Option<String>,
    pub owner_pid: Option<u32>,
    pub private_env_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildRecord {
    pub session_id: u64,
    pub repo_root: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub total_wall_ms: Option<u64>,
    pub crate_count: u32,
    pub slowest_crate_us: Option<u64>,
    pub slowest_crate_name: Option<String>,
}

/// Pre-#576 shape of [`StatusInfo`], retained so persisted-state
/// blobs or pinned-version test fixtures can still be decoded into
/// the new struct with `cook_stats` defaulting to `None`. The wire
/// path itself enforces protocol-version equality via
/// [`crate::daemon::ipc`], so this fallback is a documented "can be"
/// rather than a "is" — but the unit test in this file exercises it
/// so the path stays live.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LegacyStatusInfo {
    version: u32,
    pid: u32,
    uptime_secs: u64,
    request_count: u64,
    linked_zccache: Option<ZccacheDaemonLink>,
}

impl From<LegacyStatusInfo> for StatusInfo {
    fn from(value: LegacyStatusInfo) -> Self {
        Self {
            version: value.version,
            pid: value.pid,
            uptime_secs: value.uptime_secs,
            request_count: value.request_count,
            linked_zccache: value.linked_zccache,
            cook_stats: None,
        }
    }
}

/// Decode `bytes` into a [`StatusInfo`]. Tries the current shape
/// first; on failure falls back to the legacy shape (without
/// `cook_stats`) and lifts it via [`LegacyStatusInfo::into`]. The
/// fallback is `pub(crate)` so callers that hold raw [`StatusInfo`]
/// blobs (tests, future migrations) can use it without poking the
/// wire path.
pub(crate) fn decode_status_info_with_legacy_fallback(
    bytes: &[u8],
) -> Result<StatusInfo, bincode::Error> {
    bincode::deserialize::<StatusInfo>(bytes).or_else(|new_err| {
        bincode::deserialize::<LegacyStatusInfo>(bytes)
            .map(StatusInfo::from)
            .map_err(|_| new_err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_link() -> ZccacheDaemonLink {
        ZccacheDaemonLink {
            binary_path: "/tmp/zccache".into(),
            cache_dir: "/tmp/cache".into(),
            session_id: Some("session-1".into()),
            source: "managed".into(),
            private_daemon: false,
            daemon_name: None,
            owner_pid: None,
            private_env_keys: Vec::new(),
        }
    }

    crate::timed_test!(status_info_round_trips_with_cook_stats, {
        let info = StatusInfo {
            version: PROTOCOL_VERSION,
            pid: 4242,
            uptime_secs: 60,
            request_count: 17,
            linked_zccache: Some(sample_link()),
            cook_stats: Some(CookStats {
                entries: 3,
                total_bytes: 9_999,
                hits_this_session: 1,
            }),
        };
        let bytes = bincode::serialize(&info).expect("serialize");
        let decoded = decode_status_info_with_legacy_fallback(&bytes).expect("decode");
        assert_eq!(decoded, info);
    });

    crate::timed_test!(legacy_status_info_bytes_default_cook_stats_to_none, {
        let legacy = LegacyStatusInfo {
            version: 3,
            pid: 99,
            uptime_secs: 1,
            request_count: 0,
            linked_zccache: None,
        };
        let bytes = bincode::serialize(&legacy).expect("serialize legacy");
        let decoded = decode_status_info_with_legacy_fallback(&bytes).expect("decode");
        assert_eq!(decoded.version, 3);
        assert_eq!(decoded.pid, 99);
        assert_eq!(decoded.uptime_secs, 1);
        assert_eq!(decoded.request_count, 0);
        assert!(decoded.linked_zccache.is_none());
        assert!(
            decoded.cook_stats.is_none(),
            "cook_stats must default to None"
        );
        // And the helper view defaults to zeroes.
        assert_eq!(decoded.cook_stats_or_zero(), CookStats::default());
    });

    crate::timed_test!(cook_lookup_request_round_trips_through_bincode, {
        let req = Request::CookLookup {
            recipe_hash: [1u8; 32],
            target_triple: "x86_64-pc-windows-msvc".into(),
            profile: "release".into(),
            channel: "1.94.1".into(),
            rustc_version: "rustc 1.94.1".into(),
            origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        };
        let bytes = bincode::serialize(&req).expect("serialize");
        let decoded: Request = bincode::deserialize(&bytes).expect("deserialize");
        match decoded {
            Request::CookLookup {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
                origin_url_normalized,
            } => {
                assert_eq!(recipe_hash, [1u8; 32]);
                assert_eq!(target_triple, "x86_64-pc-windows-msvc");
                assert_eq!(profile, "release");
                assert_eq!(channel, "1.94.1");
                assert_eq!(rustc_version, "rustc 1.94.1");
                assert_eq!(
                    origin_url_normalized.as_deref(),
                    Some("https://github.com/zackees/soldr"),
                );
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    });

    crate::timed_test!(cook_hit_response_round_trips, {
        let resp = Response::CookHit {
            sha256: [0xAA; 32],
            path: "/home/runner/.soldr/cache/cook/abcd.tar.zst".into(),
            size_bytes: 4_096,
            origin_url_normalized: None,
        };
        let bytes = bincode::serialize(&resp).expect("serialize");
        let decoded: Response = bincode::deserialize(&bytes).expect("deserialize");
        match decoded {
            Response::CookHit {
                sha256,
                path,
                size_bytes,
                origin_url_normalized,
            } => {
                assert_eq!(sha256, [0xAA; 32]);
                assert!(path.ends_with(".tar.zst"));
                assert_eq!(size_bytes, 4_096);
                assert!(origin_url_normalized.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    });

    crate::timed_test!(cook_miss_response_round_trips, {
        let resp = Response::CookMiss {
            previous_origin_recipe_hashes: vec![[1u8; 32], [2u8; 32]],
        };
        let bytes = bincode::serialize(&resp).expect("serialize");
        let decoded: Response = bincode::deserialize(&bytes).expect("deserialize");
        match decoded {
            Response::CookMiss {
                previous_origin_recipe_hashes,
            } => {
                assert_eq!(previous_origin_recipe_hashes.len(), 2);
                assert_eq!(previous_origin_recipe_hashes[0], [1u8; 32]);
                assert_eq!(previous_origin_recipe_hashes[1], [2u8; 32]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    });

    crate::timed_test!(cook_record_fits_under_max_body_bytes, {
        // Conservative: long strings still fit comfortably under 64 KiB.
        let big_string = "x".repeat(1024);
        let req = Request::CookRecord {
            recipe_hash: [0; 32],
            target_triple: big_string.clone(),
            profile: big_string.clone(),
            channel: big_string.clone(),
            rustc_version: big_string.clone(),
            sha256: [0; 32],
            size_bytes: u64::MAX,
            origin_url_normalized: Some(big_string.clone()),
            cook_cmd_summary: big_string,
        };
        let bytes = bincode::serialize(&req).expect("serialize");
        assert!((bytes.len() as u32) <= MAX_BODY_BYTES);
    });

    crate::timed_test!(protocol_version_is_v4_after_cook_index, {
        assert_eq!(PROTOCOL_VERSION, 4);
    });
}
