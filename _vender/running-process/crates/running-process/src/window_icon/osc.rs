//! The OSC 1 fallback, for hosts with no native icon backend.
//!
//! # What this is, and what it is not
//!
//! `OSC 1` is the xterm "set icon name" sequence. It sets a *name*, not an
//! image — the terminal decides what, if anything, to do with it. Some window
//! managers surface it as the iconified window's label; most modern emulators
//! ignore it outright.
//!
//! So this is deliberately a fallback of last resort, and deliberately narrow:
//!
//! - It is only emitted for [`IconSource::Stock`], because a stock name is the
//!   one source that already *is* a symbolic name. Emitting bytes for a `.ico`
//!   file would mean inventing a name the caller never chose.
//! - It is reported as [`IconSupport::Degraded`], never `Available`, so a
//!   caller is told plainly that the host may ignore it. Reporting it as
//!   available would be the worst outcome: a caller that checked support,
//!   got a yes, and saw nothing change has no way to tell whether the icon
//!   failed or the terminal simply does not do icons.
//!
//! [`IconSource::Stock`]: super::IconSource
//! [`IconSupport::Degraded`]: super::IconSupport

use std::io::Write as _;

/// Render the OSC 1 sequence for `name`.
///
/// `ESC ] 1 ; <name> BEL`. The BEL terminator is used rather than `ESC \`
/// because it is what xterm documents and what the widest range of emulators
/// parse; a terminal that does not understand the sequence discards it either
/// way.
pub fn sequence(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 5);
    out.extend_from_slice(b"\x1b]1;");
    out.extend_from_slice(sanitize(name).as_bytes());
    out.push(0x07);
    out
}

/// Strip anything that would end the sequence early or start a new one.
///
/// A name carrying `ESC`, `BEL`, or a C0 control would terminate the OSC
/// mid-string and let the remainder be interpreted as terminal commands. The
/// names this crate emits are its own stock identifiers, so nothing hostile is
/// expected — but a sanitizer that only runs on untrusted input is one that
/// stops running the moment the input's provenance changes.
fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

/// Longest icon name emitted.
///
/// Bounded because the sequence goes to a terminal that has to buffer it, and
/// an unbounded name from a caller is an unbounded write into someone else's
/// parser.
const MAX_NAME_CHARS: usize = 128;

/// Write the OSC 1 sequence for `name` to stdout.
///
/// Stdout rather than stderr: the sequence is addressed to the terminal
/// attached to this process's output, and a caller that redirected stdout to a
/// file has, by doing so, said there is no terminal to talk to.
pub fn emit(name: &str) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&sequence(name))?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sequence_is_osc_1_terminated_by_bel() {
        assert_eq!(sequence("shield"), b"\x1b]1;shield\x07".to_vec());
    }

    #[test]
    fn a_control_character_cannot_terminate_the_sequence_early() {
        // Without this, everything after the injected BEL would be read by the
        // terminal as commands rather than as part of the name.
        let rendered = sequence("evil\x07]0;pwned");
        assert_eq!(rendered.iter().filter(|b| **b == 0x07).count(), 1);
        assert_eq!(rendered.last(), Some(&0x07));
    }

    #[test]
    fn an_escape_in_the_name_is_stripped() {
        let rendered = sequence("a\x1b]0;b");
        // Exactly one ESC: the one that opens the sequence we wrote.
        assert_eq!(rendered.iter().filter(|b| **b == 0x1b).count(), 1);
        assert_eq!(&rendered[..4], b"\x1b]1;");
    }

    #[test]
    fn a_long_name_is_bounded() {
        let rendered = sequence(&"x".repeat(10_000));
        // 4 bytes of prefix, MAX_NAME_CHARS of name, 1 byte of terminator.
        assert_eq!(rendered.len(), 4 + MAX_NAME_CHARS + 1);
    }

    #[test]
    fn an_empty_name_still_produces_a_well_formed_sequence() {
        assert_eq!(sequence(""), b"\x1b]1;\x07".to_vec());
    }
}
