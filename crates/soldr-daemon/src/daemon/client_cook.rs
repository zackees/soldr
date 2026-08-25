/// PR 1 cook-index client surface (#576). PR 2 (`soldr cook`) and PR 3
/// (cargo-front-door pre-flight) consume these; PR 1 ships dormant so
/// the helpers here are unused outside integration tests.
///
/// The reply is a [`CookLookupOutcome`]:
/// * `Hit { sha256, path, size_bytes, origin_url_normalized }` — PR 3
///   verifies `sha256` against the bytes at `path` before extracting.
/// * `Miss { previous_origin_recipe_hashes }` — used as a drift
///   diagnostic when the pre-flight misses.
///
/// `Err(ClientError::NotRunning)` means the daemon endpoint is not
/// reachable — caller must NOT treat this as a hard error; the hot
/// path falls through to a normal cargo run.
#[allow(clippy::too_many_arguments)]
pub fn cook_lookup(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
) -> Result<CookLookupOutcome, ClientError> {
    cook_lookup_with_branch_lineage(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_lookup_with_branch_lineage(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    origin_url_normalized: Option<String>,
    branch_lineage: Vec<String>,
) -> Result<CookLookupOutcome, ClientError> {
    let req = Request::CookLookup {
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        origin_url_normalized,
        branch_lineage,
    };
    match submit_request(sock_path, &req)? {
        Response::CookHit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
            compile_duration_ms,
            save_elapsed_ms,
        } => Ok(CookLookupOutcome::Hit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            matched_recipe_hash,
            exact_recipe_match,
            branch_name,
            compile_duration_ms,
            save_elapsed_ms,
        }),
        Response::CookMiss {
            previous_origin_recipe_hashes,
        } => Ok(CookLookupOutcome::Miss {
            previous_origin_recipe_hashes,
        }),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_lookup response: {other:?}"
        ))),
    }
}

/// Strongly-typed reply for [`cook_lookup`] / [`cook_lookup_full`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookLookupOutcome {
    Hit {
        sha256: [u8; 32],
        path: String,
        size_bytes: u64,
        origin_url_normalized: Option<String>,
        matched_recipe_hash: Option<[u8; 32]>,
        exact_recipe_match: bool,
        branch_name: Option<String>,
        compile_duration_ms: u64,
        save_elapsed_ms: u64,
    },
    Miss {
        previous_origin_recipe_hashes: Vec<[u8; 32]>,
    },
}

/// Register a cook artifact with the daemon. PR 2's `soldr cook`
/// worker calls this after writing `<sha256>.tar.zst` to
/// `~/.soldr/cache/cook/`. Blocks for the daemon's `Ack` reply
/// because PR 2 wants to know whether the indexing succeeded before
/// emitting its `soldr cook: indexed` line.
#[allow(clippy::too_many_arguments)]
pub fn cook_record(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    cook_cmd_summary: String,
) -> Result<(), ClientError> {
    cook_record_with_branch(
        sock_path,
        recipe_hash,
        target_triple,
        profile,
        channel,
        rustc_version,
        sha256,
        size_bytes,
        origin_url_normalized,
        None,
        cook_cmd_summary,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn cook_record_with_branch(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    branch_name: Option<String>,
    cook_cmd_summary: String,
) -> Result<(), ClientError> {
    cook_record_with_branch_timing(
        sock_path,
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
        0,
        0,
    )
}

/// Register a cook artifact with the wall-time observations used by the
/// restore cost gate. The compatibility wrapper above intentionally leaves
/// these at zero for old callers, which makes hydration skip conservatively.
#[allow(clippy::too_many_arguments)]
pub fn cook_record_with_branch_timing(
    sock_path: &Path,
    recipe_hash: [u8; 32],
    target_triple: String,
    profile: String,
    channel: String,
    rustc_version: String,
    sha256: [u8; 32],
    size_bytes: u64,
    origin_url_normalized: Option<String>,
    branch_name: Option<String>,
    cook_cmd_summary: String,
    compile_duration_ms: u64,
    save_elapsed_ms: u64,
) -> Result<(), ClientError> {
    let req = Request::CookRecord {
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
    };
    match submit_request(sock_path, &req)? {
        Response::Ack => Ok(()),
        Response::Error(msg) => Err(ClientError::Protocol(msg)),
        other => Err(ClientError::Protocol(format!(
            "unexpected cook_record response: {other:?}"
        ))),
    }
}

/// Fire-and-forget bump of `last_used_unix_ms` for the entry whose
/// sha256 matches. Best-effort by design: a touch failure must never
/// affect callers (eviction will simply observe stale `last_used`).
pub fn cook_touch(sock_path: &Path, sha256: [u8; 32]) -> Result<(), ClientError> {
    submit_fire_and_forget(sock_path, &Request::CookTouch { sha256 })
}
