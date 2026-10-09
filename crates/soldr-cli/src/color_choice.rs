//! The canonical stderr/stdout color rule (soldr#3437, unifying the six predicates).
//!
//! # The ruling
//!
//! ```text
//! color = NO_COLOR unset AND (GITHUB_ACTIONS is set OR the surface's own
//!                              stream is a terminal)
//! ```
//!
//! soldr#3437 found six hand-rolled "should this surface be colored"
//! predicates, and they disagreed about GitHub Actions, so one build log
//! carried green HIT/MISS lines and a green `soldr install` resolution block
//! beside a plain low-disk warning and a plain log-path summary. Same class of
//! defect as soldr#2740's five environment-truthiness parsers: every predicate
//! looked correct in isolation, the defect lived only *between* them.
//!
//! | Site | Before soldr#3437 |
//! |---|---|
//! | `cargo_front_door::disk::stderr_should_use_color` | `NO_COLOR` honored, TTY required, plain under Actions |
//! | `cargo_front_door::cache_states::use_color` | `NO_COLOR` honored, colors under Actions (soldr#2302) |
//! | `cargo_front_door::log_summary::use_color` | `NO_COLOR` honored, TTY required, explicitly plain under Actions |
//! | `cargo_front_door::output_capture::emit_zthreads_fallback_warning` | `NO_COLOR` honored, TTY required, plain under Actions |
//! | `install::plan::use_color` | `NO_COLOR` honored, colors under Actions, but read `GITHUB_ACTIONS` raw so `GITHUB_ACTIONS=false` counted as on |
//! | `ci_test::test_targets::use_color` | delegated to `cache_states` |
//!
//! ## Why each clause
//!
//! * **`NO_COLOR` wins over everything** (no-color.org): any value, including
//!   the empty string, disables — `var_os(..).is_some()`, unchanged from all
//!   six originals.
//! * **`GITHUB_ACTIONS` implies color without a terminal**: a runner's output
//!   is captured and rendered with ANSI by the viewer. Three of the six
//!   already did this (soldr#2302 argued it deliberately for the cache states,
//!   where green/yellow is the whole point of the feature on CI); the ruling
//!   adopts that as the rule rather than keeping three named exceptions.
//! * **`GITHUB_ACTIONS` is read through `core::foreign_flag`** (soldr#2740's
//!   denylist rule for variables soldr does not own), so `GITHUB_ACTIONS=false`
//!   is off. `install/plan.rs` used to treat any value, `false` included, as
//!   on — that spelling drift dies here with the predicate.
//! * **`TERM=dumb` is deliberately not consulted.** No surface checked it
//!   before this unification; adding it would be new semantics, not
//!   unification. The rule is the *union* of the existing behavior.
//!
//! # What stays per-surface
//!
//! Only **which stream** the surface paints. Every one of the six writes to
//! stderr, so [`stderr_enabled`] is the one convenience wrapper; the pure
//! [`enabled`] takes the caller's own stream-terminal bit, so a future
//! stdout surface passes `stdout().is_terminal()` instead. Nothing here
//! decides *where* output goes.
//!
//! # One paint helper
//!
//! [`GREEN`] / [`YELLOW`] / [`DIM`] / [`RESET`] and [`paint`] live here too:
//! the same six sites (plus `cook`, `cook_hydrate`, `line_endings`, `wheel`)
//! each carried their own copy of the escape constants. A surface paints with
//! [`paint`] and asks [`stderr_enabled`] for the gate; that is the whole API.

use std::io::IsTerminal;

/// ANSI green: "this was a cache hit / a prebuilt / a success".
pub(crate) const GREEN: &str = "\x1b[32m";
/// ANSI yellow: "this built / this is a warning".
pub(crate) const YELLOW: &str = "\x1b[33m";
/// Dim: a passthrough or a not-cacheable tag is *not* a failure, so it must
/// not borrow yellow.
pub(crate) const DIM: &str = "\x1b[2m";
pub(crate) const RESET: &str = "\x1b[0m";

/// The ruling itself, pure: no environment, no stream — the caller supplies
/// its own stream's terminality, which is the only thing that stays
/// per-surface.
///
/// See the module doc for why each clause exists and what it replaced.
#[must_use]
pub(crate) fn enabled(no_color_set: bool, github_actions: bool, stream_is_terminal: bool) -> bool {
    !no_color_set && (github_actions || stream_is_terminal)
}

/// The canonical decision for a **stderr** surface.
///
/// Every current color surface writes to stderr, so this is the wrapper all
/// of them call. `NO_COLOR` is presence-checked (any value, per no-color.org);
/// `GITHUB_ACTIONS` goes through soldr#2740's foreign denylist rule.
#[must_use]
pub(crate) fn stderr_enabled() -> bool {
    enabled(
        std::env::var_os("NO_COLOR").is_some(),
        crate::core::foreign_flag("GITHUB_ACTIONS"),
        std::io::stderr().is_terminal(),
    )
}

/// Wrap `text` in `color` + [`RESET`] when `use_color`, else return it
/// unchanged. The one `paint` helper; every surface used to own a copy.
#[must_use]
pub(crate) fn paint(text: &str, color: &str, use_color: bool) -> String {
    if use_color {
        format!("{color}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// [`paint`] with the canonical stderr decision already applied — the
/// "should I even ask?" one-liner for a surface that paints straight to
/// stderr.
#[must_use]
pub(crate) fn paint_stderr(text: &str, color: &str) -> String {
    paint(text, color, stderr_enabled())
}

#[cfg(test)]
#[path = "color_choice_tests.rs"]
mod tests;
