//! Request object for the compile pipeline.

use super::super::*;

pub(super) struct CompileRequest<'a> {
    pub(super) state_arc: &'a Arc<SharedState>,
    pub(super) session_id: &'a str,
    pub(super) args: &'a [String],
    pub(super) cwd: &'a Path,
    pub(super) compiler_path: &'a Path,
    pub(super) client_env: Option<Vec<(String, String)>>,
    pub(super) stdin: Vec<u8>,
    /// Streaming sink for the rustc subprocess pipes (Phase 5b2,
    /// soldr#983). When `Some`, the miss path's `run_compile_exec`
    /// pumps stdout/stderr chunks into this sink as rustc produces them
    /// instead of buffering them until `wait_with_output`. Hit paths
    /// ignore it — they don't spawn a child.
    pub(super) sink: Option<StreamingSink>,
}
