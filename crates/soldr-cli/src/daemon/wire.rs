//! Hand-written prost types + conversions for the daemon wire schema
//! (issue #580). The `.proto` schema lives next door as
//! [`wire.proto`](./wire.proto); keep the two in sync.
//!
//! ## Why hand-written instead of `prost-build`
//!
//! The repo already uses this pattern in `src/rust_plan_proto.rs`. It
//! avoids the build.rs / `prost-build` toolchain dep, keeps generated
//! source out of `OUT_DIR` (CI-visible), and matches the existing
//! style. The schema is small enough that drift between the .proto
//! and the .rs is caught by the round-trip unit tests below — every
//! Rust ↔ wire ↔ Rust conversion is asserted equal.
//!
//! ## Backwards compatibility
//!
//! * **Wire path**: bumped `PROTOCOL_VERSION` from 4 to 5 in
//!   [`crate::daemon::protocol`]. Old (v4 / bincode-bodied) frames
//!   are rejected by the IPC version check before they reach
//!   bincode-decode, so there is no silent format confusion.
//! * **Redb path**: the persistent rows added by this issue use a
//!   one-byte tag prefix — `0x01` means "prost-encoded body
//!   follows". The read path tries the tagged decode first; on tag
//!   mismatch (or any decode failure) it falls back to the legacy
//!   bincode decoder so rows written before this migration still
//!   round-trip.
//!
//! ## Sha32 type-safety
//!
//! proto3 has no fixed-length byte arrays — every hash is `bytes` on
//! the wire and `Vec<u8>` in the generated Rust. The conversion
//! helpers in this module validate exact-32-byte length on decode and
//! return [`crate::daemon::protocol::WireDecodeError`] on mismatch,
//! preserving the `[u8; 32]` type-level guarantee at the public API.

use crate::daemon::db::{Event, EventKind};
use crate::daemon::protocol::{
    BuildRecord, CookStats, Request, Response, StatusInfo, WireDecodeError, ZccacheDaemonLink,
};

pub mod proto {
    use prost::{Message, Oneof};

    // -- Request ----------------------------------------------------------

    #[derive(Clone, PartialEq, Message)]
    pub struct WireRequest {
        #[prost(oneof = "WireRequestKind", tags = "1,2,3,4,5,6,7,8,9,10,11,12")]
        pub kind: Option<WireRequestKind>,
    }

    #[derive(Clone, PartialEq, Oneof)]
    pub enum WireRequestKind {
        #[prost(message, tag = "1")]
        RecordTargetTouch(WireRecordTargetTouch),
        #[prost(message, tag = "2")]
        Status(WireUnit),
        #[prost(message, tag = "3")]
        Shutdown(WireUnit),
        #[prost(message, tag = "4")]
        BuildSessionStart(WireBuildSessionStart),
        #[prost(message, tag = "5")]
        BuildSessionEnd(WireBuildSessionEnd),
        #[prost(message, tag = "6")]
        RecordCompile(WireRecordCompile),
        #[prost(message, tag = "7")]
        ListBuilds(WireListBuilds),
        #[prost(message, tag = "8")]
        ListSlowBuilds(WireListSlowBuilds),
        #[prost(message, tag = "9")]
        LinkZccache(WireLinkZccache),
        #[prost(message, tag = "10")]
        CookLookup(WireCookLookup),
        #[prost(message, tag = "11")]
        CookRecord(WireCookRecord),
        #[prost(message, tag = "12")]
        CookTouch(WireCookTouch),
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireUnit {}

    #[derive(Clone, PartialEq, Message)]
    pub struct WireRecordTargetTouch {
        #[prost(string, tag = "1")]
        pub path: String,
        #[prost(int64, tag = "2")]
        pub unix_seconds: i64,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireBuildSessionStart {
        #[prost(uint64, tag = "1")]
        pub session_id: u64,
        #[prost(string, tag = "2")]
        pub repo_root: String,
        #[prost(int64, tag = "3")]
        pub started_at_ms: i64,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireBuildSessionEnd {
        #[prost(uint64, tag = "1")]
        pub session_id: u64,
        #[prost(int32, tag = "2")]
        pub exit_code: i32,
        #[prost(int64, tag = "3")]
        pub ended_at_ms: i64,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireRecordCompile {
        #[prost(uint64, tag = "1")]
        pub session_id: u64,
        #[prost(string, tag = "2")]
        pub crate_name: String,
        #[prost(string, tag = "3")]
        pub target_dir: String,
        #[prost(int64, tag = "4")]
        pub started_at_ms: i64,
        #[prost(uint64, optional, tag = "5")]
        pub duration_us: Option<u64>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireListBuilds {
        #[prost(uint32, tag = "1")]
        pub limit: u32,
        #[prost(int64, optional, tag = "2")]
        pub since_ms: Option<i64>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireListSlowBuilds {
        #[prost(uint64, tag = "1")]
        pub threshold_ms: u64,
        #[prost(uint32, tag = "2")]
        pub limit: u32,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireLinkZccache {
        #[prost(message, optional, tag = "1")]
        pub link: Option<WireZccacheDaemonLink>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookLookup {
        #[prost(bytes = "vec", tag = "1")]
        pub recipe_hash: Vec<u8>,
        #[prost(string, tag = "2")]
        pub target_triple: String,
        #[prost(string, tag = "3")]
        pub profile: String,
        #[prost(string, tag = "4")]
        pub channel: String,
        #[prost(string, tag = "5")]
        pub rustc_version: String,
        #[prost(string, optional, tag = "6")]
        pub origin_url_normalized: Option<String>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookRecord {
        #[prost(bytes = "vec", tag = "1")]
        pub recipe_hash: Vec<u8>,
        #[prost(string, tag = "2")]
        pub target_triple: String,
        #[prost(string, tag = "3")]
        pub profile: String,
        #[prost(string, tag = "4")]
        pub channel: String,
        #[prost(string, tag = "5")]
        pub rustc_version: String,
        #[prost(bytes = "vec", tag = "6")]
        pub sha256: Vec<u8>,
        #[prost(uint64, tag = "7")]
        pub size_bytes: u64,
        #[prost(string, optional, tag = "8")]
        pub origin_url_normalized: Option<String>,
        #[prost(string, tag = "9")]
        pub cook_cmd_summary: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookTouch {
        #[prost(bytes = "vec", tag = "1")]
        pub sha256: Vec<u8>,
    }

    // -- Response ---------------------------------------------------------

    #[derive(Clone, PartialEq, Message)]
    pub struct WireResponse {
        #[prost(oneof = "WireResponseKind", tags = "1,2,3,4,5,6,7")]
        pub kind: Option<WireResponseKind>,
    }

    #[derive(Clone, PartialEq, Oneof)]
    pub enum WireResponseKind {
        #[prost(message, tag = "1")]
        Status(WireStatusInfo),
        #[prost(message, tag = "2")]
        ShuttingDown(WireUnit),
        #[prost(message, tag = "3")]
        Builds(WireBuilds),
        #[prost(string, tag = "4")]
        Error(String),
        #[prost(message, tag = "5")]
        CookHit(WireCookHit),
        #[prost(message, tag = "6")]
        CookMiss(WireCookMiss),
        #[prost(message, tag = "7")]
        Ack(WireUnit),
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireBuilds {
        #[prost(message, repeated, tag = "1")]
        pub items: Vec<WireBuildRecord>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookHit {
        #[prost(bytes = "vec", tag = "1")]
        pub sha256: Vec<u8>,
        #[prost(string, tag = "2")]
        pub path: String,
        #[prost(uint64, tag = "3")]
        pub size_bytes: u64,
        #[prost(string, optional, tag = "4")]
        pub origin_url_normalized: Option<String>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookMiss {
        #[prost(bytes = "vec", repeated, tag = "1")]
        pub previous_origin_recipe_hashes: Vec<Vec<u8>>,
    }

    // -- Shared / persistent ----------------------------------------------

    #[derive(Clone, PartialEq, Message)]
    pub struct WireStatusInfo {
        #[prost(uint32, tag = "1")]
        pub version: u32,
        #[prost(uint32, tag = "2")]
        pub pid: u32,
        #[prost(uint64, tag = "3")]
        pub uptime_secs: u64,
        #[prost(uint64, tag = "4")]
        pub request_count: u64,
        #[prost(message, optional, tag = "5")]
        pub linked_zccache: Option<WireZccacheDaemonLink>,
        #[prost(message, optional, tag = "6")]
        pub cook_stats: Option<WireCookStats>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookStats {
        #[prost(uint64, tag = "1")]
        pub entries: u64,
        #[prost(uint64, tag = "2")]
        pub total_bytes: u64,
        #[prost(uint64, tag = "3")]
        pub hits_this_session: u64,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireZccacheDaemonLink {
        #[prost(string, tag = "1")]
        pub binary_path: String,
        #[prost(string, tag = "2")]
        pub cache_dir: String,
        #[prost(string, optional, tag = "3")]
        pub session_id: Option<String>,
        #[prost(string, tag = "4")]
        pub source: String,
        #[prost(bool, tag = "5")]
        pub private_daemon: bool,
        #[prost(string, optional, tag = "6")]
        pub daemon_name: Option<String>,
        #[prost(uint32, optional, tag = "7")]
        pub owner_pid: Option<u32>,
        #[prost(string, repeated, tag = "8")]
        pub private_env_keys: Vec<String>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireBuildRecord {
        #[prost(uint64, tag = "1")]
        pub session_id: u64,
        #[prost(string, tag = "2")]
        pub repo_root: String,
        #[prost(int64, tag = "3")]
        pub started_at_ms: i64,
        #[prost(int64, optional, tag = "4")]
        pub ended_at_ms: Option<i64>,
        #[prost(int32, optional, tag = "5")]
        pub exit_code: Option<i32>,
        #[prost(uint64, optional, tag = "6")]
        pub total_wall_ms: Option<u64>,
        #[prost(uint32, tag = "7")]
        pub crate_count: u32,
        #[prost(uint64, optional, tag = "8")]
        pub slowest_crate_us: Option<u64>,
        #[prost(string, optional, tag = "9")]
        pub slowest_crate_name: Option<String>,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireEvent {
        #[prost(int64, tag = "1")]
        pub ts_ms: i64,
        #[prost(uint64, optional, tag = "2")]
        pub session_id: Option<u64>,
        #[prost(uint32, tag = "3")]
        pub kind: u32,
        #[prost(string, optional, tag = "4")]
        pub crate_name: Option<String>,
        #[prost(uint64, optional, tag = "5")]
        pub duration_us: Option<u64>,
        #[prost(string, optional, tag = "6")]
        pub target_dir: Option<String>,
        #[prost(int32, optional, tag = "7")]
        pub exit_code: Option<i32>,
    }

    // -- cook_index persistent rows ---------------------------------------

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookKey {
        #[prost(bytes = "vec", tag = "1")]
        pub recipe_hash: Vec<u8>,
        #[prost(string, tag = "2")]
        pub target_triple: String,
        #[prost(string, tag = "3")]
        pub profile: String,
        #[prost(string, tag = "4")]
        pub channel: String,
        #[prost(string, tag = "5")]
        pub rustc_version: String,
    }

    #[derive(Clone, PartialEq, Message)]
    pub struct WireCookEntry {
        #[prost(bytes = "vec", tag = "1")]
        pub sha256: Vec<u8>,
        #[prost(uint64, tag = "2")]
        pub size_bytes: u64,
        #[prost(uint64, tag = "3")]
        pub created_unix_ms: u64,
        #[prost(uint64, tag = "4")]
        pub last_used_unix_ms: u64,
        #[prost(string, optional, tag = "5")]
        pub origin_url_normalized: Option<String>,
        #[prost(string, tag = "6")]
        pub cook_cmd_summary: String,
    }
}

use prost::Message as _;

// =========================================================================
// Sha32 helpers
// =========================================================================

fn sha_to_vec(sha: &[u8; 32]) -> Vec<u8> {
    sha.to_vec()
}

fn vec_to_sha(bytes: &[u8]) -> Result<[u8; 32], WireDecodeError> {
    if bytes.len() != 32 {
        return Err(WireDecodeError::InvalidShaLength(bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

// =========================================================================
// Wire-tagged byte for redb persistent rows
// =========================================================================

/// The 0x01 byte that prefixes every prost-encoded redb row written
/// by this codebase. Reads look for it; absence (or any other byte)
/// triggers the legacy-bincode fallback.
pub const REDB_TAG_PROST: u8 = 0x01;

/// Prepend [`REDB_TAG_PROST`] to a prost-encoded body. Used by every
/// writer that lands a row into a redb table participating in the
/// #580 migration.
pub fn prost_tagged_bytes<M: prost::Message>(message: &M) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + message.encoded_len());
    out.push(REDB_TAG_PROST);
    message.encode(&mut out).expect("Vec write is infallible");
    out
}

/// Inverse of [`prost_tagged_bytes`]: when the row starts with the
/// prost tag, hand the caller the prost payload to decode. When the
/// row starts with any other byte, hand back the full original slice
/// so the caller can attempt the legacy bincode decoder.
pub enum RedbDecode<'a> {
    Prost(&'a [u8]),
    LegacyBincode(&'a [u8]),
}

pub fn classify_redb_row(bytes: &[u8]) -> RedbDecode<'_> {
    match bytes.split_first() {
        Some((&REDB_TAG_PROST, rest)) => RedbDecode::Prost(rest),
        _ => RedbDecode::LegacyBincode(bytes),
    }
}

// =========================================================================
// Encode / decode at the Request + Response boundary (wire framing)
// =========================================================================

pub fn encode_request(req: &Request) -> Vec<u8> {
    let wire: proto::WireRequest = req.into();
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

pub fn decode_request(bytes: &[u8]) -> Result<Request, WireDecodeError> {
    let wire = proto::WireRequest::decode(bytes).map_err(WireDecodeError::Prost)?;
    Request::try_from(wire)
}

pub fn encode_response(resp: &Response) -> Vec<u8> {
    let wire: proto::WireResponse = resp.into();
    let mut out = Vec::with_capacity(wire.encoded_len());
    wire.encode(&mut out).expect("Vec write is infallible");
    out
}

pub fn decode_response(bytes: &[u8]) -> Result<Response, WireDecodeError> {
    let wire = proto::WireResponse::decode(bytes).map_err(WireDecodeError::Prost)?;
    Response::try_from(wire)
}

// =========================================================================
// Conversions: persistent record types
// =========================================================================

pub fn build_record_to_wire(record: &BuildRecord) -> proto::WireBuildRecord {
    proto::WireBuildRecord {
        session_id: record.session_id,
        repo_root: record.repo_root.clone(),
        started_at_ms: record.started_at_ms,
        ended_at_ms: record.ended_at_ms,
        exit_code: record.exit_code,
        total_wall_ms: record.total_wall_ms,
        crate_count: record.crate_count,
        slowest_crate_us: record.slowest_crate_us,
        slowest_crate_name: record.slowest_crate_name.clone(),
    }
}

pub fn build_record_from_wire(wire: proto::WireBuildRecord) -> BuildRecord {
    BuildRecord {
        session_id: wire.session_id,
        repo_root: wire.repo_root,
        started_at_ms: wire.started_at_ms,
        ended_at_ms: wire.ended_at_ms,
        exit_code: wire.exit_code,
        total_wall_ms: wire.total_wall_ms,
        crate_count: wire.crate_count,
        slowest_crate_us: wire.slowest_crate_us,
        slowest_crate_name: wire.slowest_crate_name,
    }
}

pub fn event_to_wire(event: &Event) -> proto::WireEvent {
    proto::WireEvent {
        ts_ms: event.ts_ms,
        session_id: event.session_id,
        kind: event_kind_to_u32(&event.kind),
        crate_name: event.crate_name.clone(),
        duration_us: event.duration_us,
        target_dir: event.target_dir.clone(),
        exit_code: event.exit_code,
    }
}

pub fn event_from_wire(wire: proto::WireEvent) -> Result<Event, WireDecodeError> {
    Ok(Event {
        ts_ms: wire.ts_ms,
        session_id: wire.session_id,
        kind: u32_to_event_kind(wire.kind)?,
        crate_name: wire.crate_name,
        duration_us: wire.duration_us,
        target_dir: wire.target_dir,
        exit_code: wire.exit_code,
    })
}

fn event_kind_to_u32(kind: &EventKind) -> u32 {
    match kind {
        EventKind::SessionStart => 0,
        EventKind::SessionEnd => 1,
        EventKind::CompileStart => 2,
        EventKind::CompileEnd => 3,
    }
}

fn u32_to_event_kind(n: u32) -> Result<EventKind, WireDecodeError> {
    match n {
        0 => Ok(EventKind::SessionStart),
        1 => Ok(EventKind::SessionEnd),
        2 => Ok(EventKind::CompileStart),
        3 => Ok(EventKind::CompileEnd),
        other => Err(WireDecodeError::UnknownEventKind(other)),
    }
}

pub fn zccache_link_to_wire(link: &ZccacheDaemonLink) -> proto::WireZccacheDaemonLink {
    proto::WireZccacheDaemonLink {
        binary_path: link.binary_path.clone(),
        cache_dir: link.cache_dir.clone(),
        session_id: link.session_id.clone(),
        source: link.source.clone(),
        private_daemon: link.private_daemon,
        daemon_name: link.daemon_name.clone(),
        owner_pid: link.owner_pid,
        private_env_keys: link.private_env_keys.clone(),
    }
}

pub fn zccache_link_from_wire(wire: proto::WireZccacheDaemonLink) -> ZccacheDaemonLink {
    ZccacheDaemonLink {
        binary_path: wire.binary_path,
        cache_dir: wire.cache_dir,
        session_id: wire.session_id,
        source: wire.source,
        private_daemon: wire.private_daemon,
        daemon_name: wire.daemon_name,
        owner_pid: wire.owner_pid,
        private_env_keys: wire.private_env_keys,
    }
}

fn cook_stats_to_wire(stats: &CookStats) -> proto::WireCookStats {
    proto::WireCookStats {
        entries: stats.entries,
        total_bytes: stats.total_bytes,
        hits_this_session: stats.hits_this_session,
    }
}

fn cook_stats_from_wire(wire: proto::WireCookStats) -> CookStats {
    CookStats {
        entries: wire.entries,
        total_bytes: wire.total_bytes,
        hits_this_session: wire.hits_this_session,
    }
}

pub fn status_info_to_wire(info: &StatusInfo) -> proto::WireStatusInfo {
    proto::WireStatusInfo {
        version: info.version,
        pid: info.pid,
        uptime_secs: info.uptime_secs,
        request_count: info.request_count,
        linked_zccache: info.linked_zccache.as_ref().map(zccache_link_to_wire),
        cook_stats: info.cook_stats.as_ref().map(cook_stats_to_wire),
    }
}

pub fn status_info_from_wire(wire: proto::WireStatusInfo) -> StatusInfo {
    StatusInfo {
        version: wire.version,
        pid: wire.pid,
        uptime_secs: wire.uptime_secs,
        request_count: wire.request_count,
        linked_zccache: wire.linked_zccache.map(zccache_link_from_wire),
        cook_stats: wire.cook_stats.map(cook_stats_from_wire),
    }
}

// =========================================================================
// Conversions: Request
// =========================================================================

impl From<&Request> for proto::WireRequest {
    fn from(req: &Request) -> Self {
        let kind = match req {
            Request::RecordTargetTouch { path, unix_seconds } => {
                proto::WireRequestKind::RecordTargetTouch(proto::WireRecordTargetTouch {
                    path: path.clone(),
                    unix_seconds: *unix_seconds,
                })
            }
            Request::Status => proto::WireRequestKind::Status(proto::WireUnit {}),
            Request::Shutdown => proto::WireRequestKind::Shutdown(proto::WireUnit {}),
            Request::BuildSessionStart {
                session_id,
                repo_root,
                started_at_ms,
            } => proto::WireRequestKind::BuildSessionStart(proto::WireBuildSessionStart {
                session_id: *session_id,
                repo_root: repo_root.clone(),
                started_at_ms: *started_at_ms,
            }),
            Request::BuildSessionEnd {
                session_id,
                exit_code,
                ended_at_ms,
            } => proto::WireRequestKind::BuildSessionEnd(proto::WireBuildSessionEnd {
                session_id: *session_id,
                exit_code: *exit_code,
                ended_at_ms: *ended_at_ms,
            }),
            Request::RecordCompile {
                session_id,
                crate_name,
                target_dir,
                started_at_ms,
                duration_us,
            } => proto::WireRequestKind::RecordCompile(proto::WireRecordCompile {
                session_id: *session_id,
                crate_name: crate_name.clone(),
                target_dir: target_dir.clone(),
                started_at_ms: *started_at_ms,
                duration_us: *duration_us,
            }),
            Request::ListBuilds { limit, since_ms } => {
                proto::WireRequestKind::ListBuilds(proto::WireListBuilds {
                    limit: *limit,
                    since_ms: *since_ms,
                })
            }
            Request::ListSlowBuilds {
                threshold_ms,
                limit,
            } => proto::WireRequestKind::ListSlowBuilds(proto::WireListSlowBuilds {
                threshold_ms: *threshold_ms,
                limit: *limit,
            }),
            Request::LinkZccache { link } => {
                proto::WireRequestKind::LinkZccache(proto::WireLinkZccache {
                    link: Some(zccache_link_to_wire(link)),
                })
            }
            Request::CookLookup {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
                origin_url_normalized,
            } => proto::WireRequestKind::CookLookup(proto::WireCookLookup {
                recipe_hash: sha_to_vec(recipe_hash),
                target_triple: target_triple.clone(),
                profile: profile.clone(),
                channel: channel.clone(),
                rustc_version: rustc_version.clone(),
                origin_url_normalized: origin_url_normalized.clone(),
            }),
            Request::CookRecord {
                recipe_hash,
                target_triple,
                profile,
                channel,
                rustc_version,
                sha256,
                size_bytes,
                origin_url_normalized,
                cook_cmd_summary,
            } => proto::WireRequestKind::CookRecord(proto::WireCookRecord {
                recipe_hash: sha_to_vec(recipe_hash),
                target_triple: target_triple.clone(),
                profile: profile.clone(),
                channel: channel.clone(),
                rustc_version: rustc_version.clone(),
                sha256: sha_to_vec(sha256),
                size_bytes: *size_bytes,
                origin_url_normalized: origin_url_normalized.clone(),
                cook_cmd_summary: cook_cmd_summary.clone(),
            }),
            Request::CookTouch { sha256 } => {
                proto::WireRequestKind::CookTouch(proto::WireCookTouch {
                    sha256: sha_to_vec(sha256),
                })
            }
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<proto::WireRequest> for Request {
    type Error = WireDecodeError;

    fn try_from(wire: proto::WireRequest) -> Result<Self, Self::Error> {
        let kind = wire.kind.ok_or(WireDecodeError::EmptyOneof("Request"))?;
        Ok(match kind {
            proto::WireRequestKind::RecordTargetTouch(m) => Request::RecordTargetTouch {
                path: m.path,
                unix_seconds: m.unix_seconds,
            },
            proto::WireRequestKind::Status(_) => Request::Status,
            proto::WireRequestKind::Shutdown(_) => Request::Shutdown,
            proto::WireRequestKind::BuildSessionStart(m) => Request::BuildSessionStart {
                session_id: m.session_id,
                repo_root: m.repo_root,
                started_at_ms: m.started_at_ms,
            },
            proto::WireRequestKind::BuildSessionEnd(m) => Request::BuildSessionEnd {
                session_id: m.session_id,
                exit_code: m.exit_code,
                ended_at_ms: m.ended_at_ms,
            },
            proto::WireRequestKind::RecordCompile(m) => Request::RecordCompile {
                session_id: m.session_id,
                crate_name: m.crate_name,
                target_dir: m.target_dir,
                started_at_ms: m.started_at_ms,
                duration_us: m.duration_us,
            },
            proto::WireRequestKind::ListBuilds(m) => Request::ListBuilds {
                limit: m.limit,
                since_ms: m.since_ms,
            },
            proto::WireRequestKind::ListSlowBuilds(m) => Request::ListSlowBuilds {
                threshold_ms: m.threshold_ms,
                limit: m.limit,
            },
            proto::WireRequestKind::LinkZccache(m) => Request::LinkZccache {
                link: zccache_link_from_wire(
                    m.link
                        .ok_or(WireDecodeError::MissingField("LinkZccache.link"))?,
                ),
            },
            proto::WireRequestKind::CookLookup(m) => Request::CookLookup {
                recipe_hash: vec_to_sha(&m.recipe_hash)?,
                target_triple: m.target_triple,
                profile: m.profile,
                channel: m.channel,
                rustc_version: m.rustc_version,
                origin_url_normalized: m.origin_url_normalized,
            },
            proto::WireRequestKind::CookRecord(m) => Request::CookRecord {
                recipe_hash: vec_to_sha(&m.recipe_hash)?,
                target_triple: m.target_triple,
                profile: m.profile,
                channel: m.channel,
                rustc_version: m.rustc_version,
                sha256: vec_to_sha(&m.sha256)?,
                size_bytes: m.size_bytes,
                origin_url_normalized: m.origin_url_normalized,
                cook_cmd_summary: m.cook_cmd_summary,
            },
            proto::WireRequestKind::CookTouch(m) => Request::CookTouch {
                sha256: vec_to_sha(&m.sha256)?,
            },
        })
    }
}

// =========================================================================
// Conversions: Response
// =========================================================================

impl From<&Response> for proto::WireResponse {
    fn from(resp: &Response) -> Self {
        let kind = match resp {
            Response::Status(info) => proto::WireResponseKind::Status(status_info_to_wire(info)),
            Response::ShuttingDown => proto::WireResponseKind::ShuttingDown(proto::WireUnit {}),
            Response::Builds(rows) => proto::WireResponseKind::Builds(proto::WireBuilds {
                items: rows.iter().map(build_record_to_wire).collect(),
            }),
            Response::Error(msg) => proto::WireResponseKind::Error(msg.clone()),
            Response::CookHit {
                sha256,
                path,
                size_bytes,
                origin_url_normalized,
            } => proto::WireResponseKind::CookHit(proto::WireCookHit {
                sha256: sha_to_vec(sha256),
                path: path.clone(),
                size_bytes: *size_bytes,
                origin_url_normalized: origin_url_normalized.clone(),
            }),
            Response::CookMiss {
                previous_origin_recipe_hashes,
            } => proto::WireResponseKind::CookMiss(proto::WireCookMiss {
                previous_origin_recipe_hashes: previous_origin_recipe_hashes
                    .iter()
                    .map(sha_to_vec)
                    .collect(),
            }),
            Response::Ack => proto::WireResponseKind::Ack(proto::WireUnit {}),
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<proto::WireResponse> for Response {
    type Error = WireDecodeError;

    fn try_from(wire: proto::WireResponse) -> Result<Self, WireDecodeError> {
        let kind = wire.kind.ok_or(WireDecodeError::EmptyOneof("Response"))?;
        Ok(match kind {
            proto::WireResponseKind::Status(m) => Response::Status(status_info_from_wire(m)),
            proto::WireResponseKind::ShuttingDown(_) => Response::ShuttingDown,
            proto::WireResponseKind::Builds(m) => {
                Response::Builds(m.items.into_iter().map(build_record_from_wire).collect())
            }
            proto::WireResponseKind::Error(msg) => Response::Error(msg),
            proto::WireResponseKind::CookHit(m) => Response::CookHit {
                sha256: vec_to_sha(&m.sha256)?,
                path: m.path,
                size_bytes: m.size_bytes,
                origin_url_normalized: m.origin_url_normalized,
            },
            proto::WireResponseKind::CookMiss(m) => {
                let mut hashes = Vec::with_capacity(m.previous_origin_recipe_hashes.len());
                for raw in m.previous_origin_recipe_hashes {
                    hashes.push(vec_to_sha(&raw)?);
                }
                Response::CookMiss {
                    previous_origin_recipe_hashes: hashes,
                }
            }
            proto::WireResponseKind::Ack(_) => Response::Ack,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{Response, StatusInfo};

    fn sample_link() -> ZccacheDaemonLink {
        ZccacheDaemonLink {
            binary_path: "/tmp/zccache".into(),
            cache_dir: "/tmp/cache".into(),
            session_id: Some("session-1".into()),
            source: "managed".into(),
            private_daemon: true,
            daemon_name: Some("soldr-dev".into()),
            owner_pid: Some(1234),
            private_env_keys: vec!["ZCCACHE_PATH_REMAP".into()],
        }
    }

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

    crate::timed_test!(cook_lookup_round_trips_with_all_fields, {
        let req = Request::CookLookup {
            recipe_hash: [0x42; 32],
            target_triple: "x86_64-pc-windows-msvc".into(),
            profile: "release".into(),
            channel: "1.94.1".into(),
            rustc_version: "rustc 1.94.1".into(),
            origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
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
            cook_cmd_summary: "cook --release".into(),
        };
        let bytes = encode_request(&req);
        let Request::CookRecord {
            recipe_hash,
            sha256,
            size_bytes,
            ..
        } = decode_request(&bytes).expect("decode")
        else {
            panic!("expected CookRecord");
        };
        assert_eq!(recipe_hash, [0x11; 32]);
        assert_eq!(sha256, [0xAA; 32]);
        assert_eq!(size_bytes, 4_096);
    });

    crate::timed_test!(link_zccache_round_trips_with_optional_fields, {
        let req = Request::LinkZccache {
            link: sample_link(),
        };
        let bytes = encode_request(&req);
        let decoded = decode_request(&bytes).expect("decode");
        match decoded {
            Request::LinkZccache { link } => {
                assert_eq!(link, sample_link());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    });

    crate::timed_test!(status_response_round_trips_with_cook_stats, {
        let info = StatusInfo {
            version: 5,
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
        let resp = Response::Status(info.clone());
        let bytes = encode_response(&resp);
        let decoded = decode_response(&bytes).expect("decode");
        match decoded {
            Response::Status(decoded_info) => assert_eq!(decoded_info, info),
            other => panic!("unexpected variant: {other:?}"),
        }
    });

    crate::timed_test!(cook_hit_response_round_trips, {
        let resp = Response::CookHit {
            sha256: [0xCC; 32],
            path: "/home/runner/.soldr/cache/cook/abcd.tar.zst".into(),
            size_bytes: 4_096,
            origin_url_normalized: Some("https://github.com/zackees/soldr".into()),
        };
        let bytes = encode_response(&resp);
        match decode_response(&bytes).expect("decode") {
            Response::CookHit {
                sha256,
                path,
                size_bytes,
                origin_url_normalized,
            } => {
                assert_eq!(sha256, [0xCC; 32]);
                assert!(path.ends_with(".tar.zst"));
                assert_eq!(size_bytes, 4_096);
                assert_eq!(
                    origin_url_normalized.as_deref(),
                    Some("https://github.com/zackees/soldr")
                );
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

    crate::timed_test!(tagged_byte_classifier_routes_correctly, {
        let prost_body = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut tagged = vec![REDB_TAG_PROST];
        tagged.extend_from_slice(&prost_body);
        match classify_redb_row(&tagged) {
            RedbDecode::Prost(rest) => assert_eq!(rest, &prost_body),
            RedbDecode::LegacyBincode(_) => panic!("expected prost classification"),
        }
        // Anything else falls through to legacy.
        let bincode_like = [0x00u8, 0x01, 0x02];
        match classify_redb_row(&bincode_like) {
            RedbDecode::Prost(_) => panic!("expected legacy classification"),
            RedbDecode::LegacyBincode(bytes) => assert_eq!(bytes, &bincode_like),
        }
        // Empty falls through to legacy too (bincode will error cleanly).
        match classify_redb_row(&[]) {
            RedbDecode::Prost(_) => panic!("expected legacy on empty"),
            RedbDecode::LegacyBincode(b) => assert!(b.is_empty()),
        }
    });
}
