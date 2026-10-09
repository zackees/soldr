//! Unit tests for the canonical color rule (soldr#3437).
//!
//! Three layers, because the defect this issue exists for was *between*
//! predicates rather than inside any one of them:
//!
//! 1. the pure truth table of [`enabled`] — the whole ruling;
//! 2. [`install_and_cache_summary_agree_under_github_actions`] — the issue's
//!    observable divergence, driven through both surfaces' real renderers;
//! 3. `tests/guards/color_predicate_guard.rs` — a source scan proving no
//!    surface re-derives the decision, which is the part a pure test cannot
//!    see.

use super::*;

/// `NO_COLOR` is the top clause: any value, in any environment, disables.
#[test]
fn no_color_beats_everything() {
    for github_actions in [false, true] {
        for stream_is_terminal in [false, true] {
            assert!(
                !enabled(true, github_actions, stream_is_terminal),
                "NO_COLOR must win over GITHUB_ACTIONS={github_actions} \
                 terminal={stream_is_terminal}"
            );
        }
    }
}

/// The half of the ruling that made the divergence visible: a runner's log is
/// captured and rendered with ANSI by the viewer, so no terminal is required.
#[test]
fn github_actions_colors_a_stream_that_is_not_a_terminal() {
    assert!(
        enabled(false, true, false),
        "GITHUB_ACTIONS implies color even when stderr is a pipe"
    );
    assert!(enabled(false, true, true));
}

/// Without Actions, the surface's own stream is the whole answer.
#[test]
fn otherwise_the_stream_itself_decides() {
    assert!(enabled(false, false, true));
    assert!(!enabled(false, false, false));
}

/// Color is decoration only: `paint` with the flag off must be the input,
/// byte for byte, and with it on must be exactly one wrapper pair.
#[test]
fn paint_wraps_or_returns_the_input_verbatim() {
    assert_eq!(paint("warning", YELLOW, false), "warning");
    assert!(!paint("warning", YELLOW, false).contains('\x1b'));
    assert_eq!(
        paint("warning", YELLOW, true),
        format!("{YELLOW}warning{RESET}")
    );
}

fn install_fixture() -> crate::install::plan::ResolvedInstall {
    use crate::install::plan::ResolvedInstall;
    use crate::install::refs::{Form, Ref};
    use crate::install::target::InstallTarget;

    ResolvedInstall {
        name: "example".to_string(),
        target: InstallTarget::GitHub {
            host: "github.com".to_string(),
            owner: "zackees".to_string(),
            repo: "example".to_string(),
            url_ref: None,
            url_release: None,
            run_id: None,
        },
        git_ref: Ref::Tag("v1.0.0".to_string()),
        sha: "9f2c1ab3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9".to_string(),
        release: None,
        release_note: Some("latest release v1.0.0".to_string()),
        form: Form::Auto,
        triple: "x86_64-unknown-linux-gnu".to_string(),
        debug: false,
        bins: Vec::new(),
        features: Vec::new(),
        locked: false,
        install_root: std::path::PathBuf::from("/home/user/.soldr/bin/installed"),
    }
}

fn cache_summary_fixture() -> crate::daemon::protocol::BuildCacheSummary {
    crate::daemon::protocol::BuildCacheSummary {
        hits: 8,
        misses: 2,
        non_cacheable: 1,
        errors: 0,
        compilations: 10,
        time_saved_ms: 1_500,
    }
}

/// soldr#3437's observable consequence, as a regression test.
///
/// The issue's environment: `GITHUB_ACTIONS` set, `NO_COLOR` unset, stderr is
/// *not* a terminal (a runner captures output). Before the unification the
/// `soldr install` resolution block was green there while other surfaces
/// printed plain, because each owned its own predicate. Both surfaces now
/// resolve color from [`stderr_enabled`]'s rule and render from the value it
/// returns, so they agree by construction — and the guard in
/// `tests/guards/color_predicate_guard.rs` is what keeps a seventh predicate
/// from reappearing to break that.
#[test]
fn install_and_cache_summary_agree_under_github_actions() {
    let color = enabled(
        false, /* github_actions */ true, /* stream_is_terminal */ false,
    );
    assert!(
        color,
        "the issue's environment (GITHUB_ACTIONS, no NO_COLOR, no TTY) must colorize"
    );

    let install_block = crate::install::plan::resolution_lines(
        &install_fixture(),
        &crate::install::plan::AcquisitionPlan::LocalPath("/src/example".into()),
        color,
    )
    .join("\n");
    assert!(
        install_block.contains('\x1b'),
        "the install resolution block must be colored under GITHUB_ACTIONS: {install_block}"
    );

    let cache_line =
        crate::cargo_front_door::cache_states::cache_stats_message(&cache_summary_fixture(), color)
            .expect("hits + misses > 0, so the summary exists");
    assert!(
        cache_line.contains('\x1b'),
        "the cache summary must be colored under GITHUB_ACTIONS: {cache_line}"
    );

    // Same decision in, same answer out: with the flag off both are plain, so
    // the only way the two can differ is the predicate — and there is one.
    let plain_install = crate::install::plan::resolution_lines(
        &install_fixture(),
        &crate::install::plan::AcquisitionPlan::LocalPath("/src/example".into()),
        false,
    )
    .join("\n");
    assert!(!plain_install.contains('\x1b'), "{plain_install}");
    let plain_cache =
        crate::cargo_front_door::cache_states::cache_stats_message(&cache_summary_fixture(), false)
            .expect("summary exists");
    assert!(!plain_cache.contains('\x1b'), "{plain_cache}");
    assert_eq!(
        install_block.contains('\x1b'),
        cache_line.contains('\x1b'),
        "install and cache-summary surfaces must resolve color identically"
    );
}
