//! End-to-end `file:line` resolution through the real pipeline (#803).
//!
//! The unit tests in `object_symbols` prove the DWARF line table can answer a
//! lookup. This proves the whole path does: a capture goes in, `symbolize()`
//! resolves it, `render_text()` prints it, and a real `file:line` comes out.
//!
//! That distinction matters here more than usual. Every piece of this path
//! already existed and was individually correct — the wire type carried `file`
//! and `line`, the renderer already formatted `(file:line)` — and the feature
//! still did not work, because nothing populated the fields. A test at either
//! end alone would have passed throughout.
//!
//! # Why the capture is synthetic
//!
//! The addresses come from the test binary's own symbol table rather than from
//! a live stack walk. A real walk would drag in the capture side (thread
//! suspension, unwinding) which has its own tests and its own flakiness; what
//! is under test here is resolution, and a synthesised frame exercises exactly
//! that with an address whose correct answer is known in advance.

// The line-number path is Unix-only: the `pdb` crate panics iterating line
// programs on PDBs rustc produces, so Windows resolves names only (#803).
#![cfg(not(target_os = "windows"))]

use running_process_probe_worker::render::render_text;
use running_process_probe_worker::symbolize::symbolize;
use running_process_probe_worker::wire::{
    CaptureFormat, DiscoveryConfig, ModuleRef, RawCapture, RawFrame, RawThread,
};

/// Build a capture naming this test binary, with one frame at `offset`.
fn capture_at(offset: u64) -> RawCapture {
    let exe = std::env::current_exe().expect("current exe");
    RawCapture {
        format: CaptureFormat::CooperativeFrames,
        discovery: DiscoveryConfig::default(),
        modules: vec![ModuleRef {
            name: exe
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path_hint: Some(exe.display().to_string()),
            ..ModuleRef::default()
        }],
        threads: vec![RawThread {
            os_tid: 1,
            name: Some("probe".to_string()),
            frames: vec![RawFrame {
                module_index: 0,
                relative_address: offset,
            }],
            // `..default()` for the rest: `RawThread` also carries the
            // interpreter frames of a mixed-mode capture, and this fixture is
            // about native resolution. Spelling every field would make the
            // test fail to compile each time the wire type grows one.
            ..RawThread::default()
        }],
    }
}

/// An address that has BOTH a symbol and a line record.
///
/// Both, because the renderer only prints `file:line` for a frame it
/// considers `Resolved`, and resolution means a function name. Offset 0 has a
/// line record and no symbol, so an earlier version of this test picked it and
/// asserted against `<no symbols for this module>`.
fn a_resolvable_offset() -> Option<u64> {
    use running_process_probe_worker::object_symbols::{LineTable, SymbolTable};
    let exe = std::env::current_exe().ok()?;
    let lines = LineTable::from_path(&exe)?;
    let symbols = SymbolTable::from_object_path(&exe)?;
    (0..2_000_000u64)
        .step_by(16)
        .find(|o| lines.lookup(*o).is_some() && symbols.lookup(*o).is_some())
}

/// Whether the caller set the opt-in for this process.
///
/// Read, never written. An earlier version had one test `set_var` and a
/// sibling `remove_var`; cargo runs integration tests as threads of one
/// process, so they raced and the sibling saw the first test's value. The
/// harness sets the variable and runs the suite twice instead — the same
/// split used for the unit-level gate test, for the same reason.
fn opt_in_is_set() -> bool {
    std::env::var_os("RUNNING_PROCESS_PROBE_LINE_NUMBERS").is_some_and(|v| v != "0")
}

#[test]
fn the_rendered_stack_matches_whether_the_opt_in_is_set() {
    let Some(offset) = a_resolvable_offset() else {
        eprintln!("skipping: no address in this binary has both a symbol and a line record");
        return;
    };

    let report = symbolize(&capture_at(offset)).expect("symbolize a self-capture");
    let text = render_text(&report);
    let frame = &report.threads[0].frames[0];

    if opt_in_is_set() {
        // The whole point of #803: an operator reading a rendered stack sees
        // the source location, not just module+offset. Asserted on the
        // rendered text because that is what they actually read — it catches a
        // renderer that drops the data as surely as a resolver that never
        // produced it.
        assert!(
            text.contains(".rs:"),
            "opt-in is set but the rendered stack carries no file:line:
{text}"
        );
        assert!(frame.file.is_some() && frame.line.is_some());
    } else {
        // Without the opt-in no `.debug_line` parsing happens, so the frame
        // stays at name + offset. If this ever carries a line, the flag has
        // stopped gating anything and every capture silently pays for line
        // resolution.
        assert!(
            frame.file.is_none() && frame.line.is_none(),
            "line resolution ran without the opt-in: {:?}:{:?}",
            frame.file,
            frame.line
        );
    }
}
