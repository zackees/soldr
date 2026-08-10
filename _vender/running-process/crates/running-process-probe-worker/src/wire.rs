//! The capture-in / report-out schema shared with the daemon (#637).
//!
//! # Module + offset, never absolute addresses
//!
//! A raw frame is `(module_index, relative_address)`. Absolute addresses never
//! cross this boundary, which makes symbolization ASLR-independent: the same
//! capture symbolizes identically on a machine where the module loaded
//! somewhere else, and a report can be re-symbolized later against the same
//! build without knowing where it once ran.
//!
//! # Python frames arrive already resolved
//!
//! `sys._current_frames()` yields file/line/function directly, so interpreter
//! frames need no symbolization and are copied through verbatim. They travel
//! in the same [`RawThread`] as the native frames so the per-thread pairing the
//! client established survives the round trip.

use serde::{Deserialize, Serialize};

/// What kind of capture the payload carries.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureFormat {
    /// Frames the client already unwound; the worker only resolves names.
    #[default]
    CooperativeFrames,
    /// A crash minidump the worker must parse itself. Landing with S7.
    Minidump,
}

/// One module the capture refers to.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModuleRef {
    /// Display name, e.g. `_native.pyd`.
    pub name: String,
    /// Where the module was loaded when captured.
    ///
    /// Recorded for provenance only. It is deliberately *not* used to resolve
    /// frames — see the module docs on ASLR independence.
    #[serde(default)]
    pub base_avma: u64,
    /// Compiler/linker identity of the binary, if known.
    #[serde(default)]
    pub code_id: Option<String>,
    /// Identity of the matching symbol file (PDB GUID+age, build id, UUID).
    #[serde(default)]
    pub debug_id: Option<String>,
    /// Native symbol filename captured from the loaded module metadata.
    #[serde(default)]
    pub debug_file: Option<String>,
    /// Where the binary was on disk, if the capturing process knew.
    #[serde(default)]
    pub path_hint: Option<String>,
    /// Extracted embedded-symbol payload selected inside the worker.
    ///
    /// The platform worker sets this to the module itself for native embedded
    /// symbols or to worker-private `.gnu_debugdata` extraction output. It is
    /// considered before external sources and still identity-checked.
    #[serde(default)]
    pub embedded_symbol_path: Option<String>,
}

/// Capture-level discovery declarations copied from the daemon-authoritative
/// ARMED registration.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct DiscoveryConfig {
    /// Optional process-level symbol manifest.
    #[serde(default)]
    pub registered_manifest: Option<String>,
    /// Explicit symbol files or directories.
    #[serde(default)]
    pub registered_symbol_paths: Vec<String>,
}

/// One native frame: a module and an offset into it.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RawFrame {
    /// Index into [`RawCapture::modules`].
    pub module_index: u32,
    /// Offset of the return address from the module's base.
    pub relative_address: u64,
}

/// One interpreter frame, already resolved by the client.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PyFrame {
    /// Source file.
    pub file: String,
    /// Line number.
    pub line: u32,
    /// Function name.
    pub func: String,
}

/// One captured thread.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RawThread {
    /// OS thread id. The join key between native and interpreter frames.
    pub os_tid: u64,
    /// Thread name, if the capturing process knew one.
    #[serde(default)]
    pub name: Option<String>,
    /// Native frames, innermost first.
    #[serde(default)]
    pub frames: Vec<RawFrame>,
    /// Interpreter frames, passed through untouched.
    #[serde(default)]
    pub py_frames: Vec<PyFrame>,
}

/// A whole capture: what the daemon hands the worker.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct RawCapture {
    /// Which path the worker should take.
    #[serde(default)]
    pub format: CaptureFormat,
    /// Registration-carried symbol discovery declarations.
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Modules the frames refer to.
    #[serde(default)]
    pub modules: Vec<ModuleRef>,
    /// Captured threads.
    #[serde(default)]
    pub threads: Vec<RawThread>,
}

/// How well a frame could be resolved.
///
/// The distinction matters for trust: `RawOnly` means "we know the module but
/// not the function", `ModuleUnknown` means "we cannot even attribute this
/// address". Neither is ever reported as a guessed name.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameStatus {
    /// A function name was found.
    Resolved,
    /// The module is known; no symbols were available for it.
    #[default]
    RawOnly,
    /// The frame's module index does not name a module in this capture.
    ModuleUnknown,
}

/// An inlined call site expanded out of one physical frame.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct InlineFrame {
    /// Inlined function name.
    pub function: String,
    /// Source file, if the symbol file carried line info.
    #[serde(default)]
    pub file: Option<String>,
    /// Line number, if known.
    #[serde(default)]
    pub line: Option<u32>,
}

/// One symbolized native frame.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SymFrame {
    /// Owning module's name, or a placeholder when unattributable.
    pub module: String,
    /// Offset within the module. Always preserved, whatever the status — a
    /// report with no symbols is still useful if the offsets survive.
    pub relative_address: u64,
    /// Resolved function name, if one was found.
    #[serde(default)]
    pub function: Option<String>,
    /// Source file, if known.
    #[serde(default)]
    pub file: Option<String>,
    /// Line number, if known.
    #[serde(default)]
    pub line: Option<u32>,
    /// Call sites inlined into this frame, outermost last.
    #[serde(default)]
    pub inline_frames: Vec<InlineFrame>,
    /// How far resolution got.
    pub status: FrameStatus,
}

/// One thread's symbolized frames.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SymThread {
    /// OS thread id, carried through unchanged.
    pub os_tid: u64,
    /// Thread name, carried through unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// Symbolized native frames.
    pub frames: Vec<SymFrame>,
    /// Interpreter frames, byte-for-byte as they arrived.
    pub py_frames: Vec<PyFrame>,
}

/// Why a module's symbols were or were not usable.
///
/// Reported per module because "no symbols" alone conflates situations that
/// need different responses: a build that shipped without symbols is expected,
/// while a file that failed verification means the *wrong* symbols are sitting
/// where the right ones belong — which keeps producing empty reports until
/// someone is told.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSymbolStatus {
    /// Symbols were found and verified against this build.
    Resolved,
    /// No symbol file of the expected name existed anywhere searched.
    #[default]
    NotFound,
    /// Files existed but none described this build.
    Mismatched,
    /// The image carries no debug directory — a stripped build.
    NoDebugDirectory,
    /// This platform has no symbol reader.
    Unsupported,
}

/// What became of one module's symbols.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ModuleReport {
    /// Module name as it appeared in the capture.
    pub name: String,
    /// How the symbol lookup ended.
    pub status: ModuleSymbolStatus,
    /// Symbol file actually used, when one was.
    #[serde(default)]
    pub symbol_file: Option<String>,
    /// Which deterministic discovery tier supplied `symbol_file`.
    #[serde(default)]
    pub symbol_source: Option<crate::discovery::DiscoverySource>,
    /// Candidates that existed but failed verification.
    ///
    /// One is a stale copy; several suggest a search path aimed at the wrong
    /// tree.
    #[serde(default)]
    pub rejected_candidates: usize,
}

/// What the worker writes to stdout.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SymbolReport {
    /// Symbolized threads, in capture order.
    pub threads: Vec<SymThread>,
    /// One entry per module the capture referenced, in capture order.
    #[serde(default)]
    pub modules: Vec<ModuleReport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absent optional fields must decode, so a minimal producer stays valid
    /// as the schema grows.
    #[test]
    fn a_minimal_capture_decodes() {
        let capture: RawCapture = serde_json::from_str(r#"{"threads":[{"os_tid":7}]}"#).unwrap();
        assert_eq!(capture.format, CaptureFormat::CooperativeFrames);
        assert_eq!(capture.threads.len(), 1);
        assert_eq!(capture.threads[0].os_tid, 7);
        assert!(capture.threads[0].frames.is_empty());
    }

    #[test]
    fn a_capture_round_trips_through_json() {
        let capture = RawCapture {
            format: CaptureFormat::CooperativeFrames,
            discovery: Default::default(),
            modules: vec![ModuleRef {
                name: "_native.pyd".into(),
                base_avma: 0x7fff_0000,
                debug_id: Some("ABCD".into()),
                ..Default::default()
            }],
            threads: vec![RawThread {
                os_tid: 42,
                name: Some("worker".into()),
                frames: vec![RawFrame {
                    module_index: 0,
                    relative_address: 0x1234,
                }],
                py_frames: vec![PyFrame {
                    file: "t.py".into(),
                    line: 9,
                    func: "main".into(),
                }],
            }],
        };
        let text = serde_json::to_string(&capture).unwrap();
        assert_eq!(serde_json::from_str::<RawCapture>(&text).unwrap(), capture);
    }

    /// Status names are part of the wire; renaming a variant would silently
    /// break a consumer, so the spelling is pinned.
    #[test]
    fn frame_status_uses_stable_wire_names() {
        assert_eq!(
            serde_json::to_string(&FrameStatus::ModuleUnknown).unwrap(),
            r#""module_unknown""#
        );
        assert_eq!(
            serde_json::to_string(&FrameStatus::RawOnly).unwrap(),
            r#""raw_only""#
        );
        assert_eq!(
            serde_json::to_string(&FrameStatus::Resolved).unwrap(),
            r#""resolved""#
        );
    }
}
