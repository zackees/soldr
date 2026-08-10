//! Off-process symbolization for probe captures (#637).
//!
//! # Why this is a separate process
//!
//! Symbol files are attacker-adjacent input: a PDB, DWARF section, or minidump
//! can be malformed in ways that crash a parser outright rather than returning
//! an error. Isolation here is therefore a **process** boundary, not a
//! `catch_unwind` — a parser that segfaults takes down only this PID, and the
//! daemon observes the exit status and carries on.
//!
//! That the isolation is link-time as well as run-time is the reason this is
//! its own crate: the daemon binary does not link the parsers at all, so no
//! future refactor can quietly pull them back into the long-lived process.
//!
//! # Never fabricate a symbol
//!
//! Every degradation path preserves the module and offset and reports a
//! [`wire::FrameStatus`] saying how far resolution got. A wrong function name
//! is worse than no name: it sends whoever reads the report looking in the
//! wrong place, and nothing in the output would contradict them.

#![deny(missing_docs)]

pub mod discovery;
pub mod line_numbers;
#[cfg(not(target_os = "windows"))]
pub mod object_symbols;
#[cfg(target_os = "windows")]
pub mod pdb_symbols;
pub mod render;
pub mod symbolize;
pub mod wire;

pub use render::render_text;
pub use symbolize::{symbolize, SymbolizeError};
pub use wire::{RawCapture, SymbolReport};
