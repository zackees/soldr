//! Streaming artifact download (S13 / #642).
//!
//! This endpoint is the reason the HTTP surface exists at all. The control
//! socket buffers a whole frame before parsing it and caps that at 16 MiB, so
//! a minidump — routinely far larger — cannot travel over it. Streaming turns
//! artifact size into a *throughput* question instead of a memory one: the
//! daemon holds one `ReaderStream` chunk at a time, so a 2 GiB dump and a
//! 2 MiB dump cost the same resident bytes.
//!
//! # The id is opaque on purpose
//!
//! `{id}` is a database row id, resolved by the crash store into a path the
//! daemon already wrote. It is never joined onto a caller-supplied string, so
//! there is no traversal to defend against — not because the input is
//! sanitized, but because no caller-supplied path component ever reaches the
//! filesystem.
//!
//! # The fetch is pinned
//!
//! `begin_fetch` takes a reference on the row for the life of the guard, and
//! GC serializes with that. Without it, a download of a 2 GiB artifact and a
//! retention sweep could overlap, and the sweep would delete the file out from
//! under the reader — on Windows, failing the unlink; on Unix, silently
//! truncating what the caller receives to whatever had already been sent.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio_util::io::ReaderStream;

use crate::http::HttpState;

/// `GET /v1/artifacts/{id}` — stream one artifact.
pub async fn download(State(state): State<HttpState>, Path(id): Path<i64>) -> Response {
    let Some(store) = state.ops().crash_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this daemon has no artifact store",
        )
            .into_response();
    };

    // Resolve under the store's lock, then release it. Holding the store lock
    // for the whole download would block every crash query for as long as the
    // transfer takes, and a 2 GiB artifact is a long time.
    //
    // What survives the guard is the *file handle it validated*, duplicated
    // here — not the path. Reopening by path afterwards would re-run every
    // check against whatever the name resolves to by then, which is a
    // time-of-check/time-of-use window on the one input a caller might want
    // to swap. An open handle has no such window: it refers to the object
    // that passed the checks, and on Unix it stays readable even if a
    // retention sweep unlinks the name underneath it.
    let (file, len) = match store.begin_fetch(id) {
        Ok(Some(guard)) => {
            let len = guard.file().metadata().map(|m| m.len()).unwrap_or(0);
            match guard.file().try_clone() {
                Ok(file) => (file, len),
                Err(error) => {
                    return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
                }
            }
        }
        // The row is gone, or its bytes are. From the caller's side the
        // artifact is simply not there, which is a legitimate outcome after a
        // retention sweep rather than a daemon fault.
        Ok(None) => return (StatusCode::NOT_FOUND, "no artifact with that id").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let file = tokio::fs::File::from_std(file);

    // 8 KiB chunks. The whole point: resident memory is independent of
    // artifact size.
    let body = Body::from_stream(ReaderStream::new(file));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len)
        // The filename is built from the id, never from stored text, so a
        // crafted class or signature cannot inject a header.
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"probe-artifact-{id}.bin\""),
        )
        .body(body)
        .unwrap_or_else(|error| {
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        })
}
