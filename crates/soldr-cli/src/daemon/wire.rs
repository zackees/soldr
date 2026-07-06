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
    BuildRecord, CompileRequest, CompileResponseBody, CompileStatsInfo, CookStats, Request,
    Response, StatusInfo, WireDecodeError, ZccacheDaemonLink,
};

#[path = "wire_proto.rs"]
pub mod proto;

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

fn vec_to_optional_sha(bytes: &[u8]) -> Result<Option<[u8; 32]>, WireDecodeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    vec_to_sha(bytes).map(Some)
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

fn compile_stats_to_wire(info: &CompileStatsInfo) -> proto::WireCompileStats {
    proto::WireCompileStats {
        total_compilations: info.total_compilations,
        cache_hits: info.cache_hits,
        cache_misses: info.cache_misses,
        non_cacheable: info.non_cacheable,
        compile_errors: info.compile_errors,
        time_saved_ms: info.time_saved_ms,
    }
}

fn compile_stats_from_wire(wire: proto::WireCompileStats) -> CompileStatsInfo {
    CompileStatsInfo {
        total_compilations: wire.total_compilations,
        cache_hits: wire.cache_hits,
        cache_misses: wire.cache_misses,
        non_cacheable: wire.non_cacheable,
        compile_errors: wire.compile_errors,
        time_saved_ms: wire.time_saved_ms,
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
        compile_backend: info.compile_backend.clone(),
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
        compile_backend: wire.compile_backend,
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
            Request::FlushCaches => proto::WireRequestKind::FlushCaches(proto::WireUnit {}),
            Request::CompileStats => proto::WireRequestKind::CompileStats(proto::WireUnit {}),
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
                branch_lineage,
            } => proto::WireRequestKind::CookLookup(proto::WireCookLookup {
                recipe_hash: sha_to_vec(recipe_hash),
                target_triple: target_triple.clone(),
                profile: profile.clone(),
                channel: channel.clone(),
                rustc_version: rustc_version.clone(),
                origin_url_normalized: origin_url_normalized.clone(),
                branch_lineage: branch_lineage.clone(),
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
                branch_name,
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
                branch_name: branch_name.clone(),
            }),
            Request::CookTouch { sha256 } => {
                proto::WireRequestKind::CookTouch(proto::WireCookTouch {
                    sha256: sha_to_vec(sha256),
                })
            }
            Request::Compile(req) => proto::WireRequestKind::Compile(compile_request_to_wire(req)),
        };
        Self { kind: Some(kind) }
    }
}

fn compile_request_to_wire(req: &CompileRequest) -> proto::WireCompileRequest {
    proto::WireCompileRequest {
        args: req.args.clone(),
        cwd: req.cwd.clone(),
        env: req
            .env
            .iter()
            .map(|(k, v)| proto::WireEnvEntry {
                key: k.clone(),
                value: v.clone(),
            })
            .collect(),
        stdin: req.stdin.clone(),
    }
}

fn compile_request_from_wire(wire: proto::WireCompileRequest) -> CompileRequest {
    CompileRequest {
        args: wire.args,
        cwd: wire.cwd,
        env: wire.env.into_iter().map(|e| (e.key, e.value)).collect(),
        stdin: wire.stdin,
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
            proto::WireRequestKind::FlushCaches(_) => Request::FlushCaches,
            proto::WireRequestKind::CompileStats(_) => Request::CompileStats,
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
                branch_lineage: m.branch_lineage,
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
                branch_name: m.branch_name,
                cook_cmd_summary: m.cook_cmd_summary,
            },
            proto::WireRequestKind::CookTouch(m) => Request::CookTouch {
                sha256: vec_to_sha(&m.sha256)?,
            },
            proto::WireRequestKind::Compile(m) => Request::Compile(compile_request_from_wire(m)),
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
                matched_recipe_hash,
                exact_recipe_match,
                branch_name,
            } => proto::WireResponseKind::CookHit(proto::WireCookHit {
                sha256: sha_to_vec(sha256),
                path: path.clone(),
                size_bytes: *size_bytes,
                origin_url_normalized: origin_url_normalized.clone(),
                matched_recipe_hash: matched_recipe_hash
                    .as_ref()
                    .map(sha_to_vec)
                    .unwrap_or_default(),
                exact_recipe_match: *exact_recipe_match,
                branch_name: branch_name.clone(),
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
            Response::Compile(body) => {
                proto::WireResponseKind::CompileResponse(proto::WireCompileResponse {
                    exit_code: body.exit_code,
                    stdout: body.stdout.clone(),
                    stderr: body.stderr.clone(),
                    cached: body.cached,
                    cache_outcome: body.cache_outcome,
                })
            }
            Response::CompileStdoutChunk(bytes) => {
                proto::WireResponseKind::CompileStdoutChunk(proto::WireCompileStdoutChunk {
                    bytes: bytes.clone(),
                })
            }
            Response::CompileStderrChunk(bytes) => {
                proto::WireResponseKind::CompileStderrChunk(proto::WireCompileStderrChunk {
                    bytes: bytes.clone(),
                })
            }
            Response::CompileDone {
                exit_code,
                cached,
                cache_outcome,
                compile_id,
            } => proto::WireResponseKind::CompileDone(proto::WireCompileDone {
                exit_code: *exit_code,
                cached: *cached,
                cache_outcome: *cache_outcome,
                compile_id: compile_id.clone(),
            }),
            Response::CompileStats(info) => {
                proto::WireResponseKind::CompileStats(compile_stats_to_wire(info))
            }
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
                matched_recipe_hash: vec_to_optional_sha(&m.matched_recipe_hash)?,
                exact_recipe_match: m.exact_recipe_match,
                branch_name: m.branch_name,
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
            proto::WireResponseKind::CompileResponse(m) => Response::Compile(CompileResponseBody {
                exit_code: m.exit_code,
                stdout: m.stdout,
                stderr: m.stderr,
                cached: m.cached,
                cache_outcome: m.cache_outcome,
            }),
            proto::WireResponseKind::CompileStdoutChunk(m) => Response::CompileStdoutChunk(m.bytes),
            proto::WireResponseKind::CompileStderrChunk(m) => Response::CompileStderrChunk(m.bytes),
            proto::WireResponseKind::CompileDone(m) => Response::CompileDone {
                exit_code: m.exit_code,
                cached: m.cached,
                cache_outcome: m.cache_outcome,
                compile_id: m.compile_id,
            },
            proto::WireResponseKind::CompileStats(m) => {
                Response::CompileStats(compile_stats_from_wire(m))
            }
        })
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
