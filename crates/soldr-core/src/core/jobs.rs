//! Soldr-owned compile concurrency limit (soldr#1761).
//!
//! # Why this exists
//!
//! The real gate on concurrent rustc processes is the embedded zccache
//! daemon's semaphore, and soldr never set it: `zccache_embedded.rs`
//! passed `ServiceLimits::default()`, so the vendored default always
//! governed. The only knob was `ZCCACHE_MAX_PARALLEL_COMPILES` — a
//! zccache-namespaced variable that has to be present in the
//! environment the *long-lived daemon* inherited, which is easy to get
//! wrong and impossible to discover from soldr's own surface.
//!
//! Worse, two layers sized themselves from two different expressions:
//! zccache's semaphore defaulted to `available_parallelism() - 1` while
//! soldr's outer `expected_compile_slots()` defaulted to
//! `available_parallelism()` with no `- 1`. The admission queue and the
//! thing it admits into disagreed about how many slots existed.
//!
//! This module is the single resolution point. Both layers now size
//! from [`resolve_compile_jobs`], so there is exactly one number.
//!
//! # Precedence
//!
//! 1. `SOLDR_JOBS` — soldr's own knob.
//! 2. `[jobs] max_parallel_compiles` in `config.toml`.
//! 3. `ZCCACHE_MAX_PARALLEL_COMPILES` — compatibility with the
//!    pre-#1761 setup, so existing configurations keep working.
//! 4. [`default_compile_jobs`].
//!
//! Values are clamped to at least 1: a limit of zero would deadlock
//! every compile, and silently correcting it beats refusing to build.

use serde::Deserialize;

/// `SOLDR_JOBS` — soldr's own compile-concurrency knob.
pub const SOLDR_JOBS_ENV_VAR: &str = "SOLDR_JOBS";

/// Pre-#1761 zccache-namespaced knob, still honored as a fallback.
pub const ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR: &str = "ZCCACHE_MAX_PARALLEL_COMPILES";

/// `[jobs]` section of `config.toml`.
///
/// ```toml
/// [jobs]
/// max_parallel_compiles = 10
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct JobsConfig {
    /// Concurrent rustc processes the embedded compile service admits.
    /// `None` falls through to the next precedence tier.
    #[serde(default)]
    pub max_parallel_compiles: Option<usize>,
}

/// Where a resolved limit came from. Surfaced so diagnostics can say
/// *why* a machine is running the concurrency it is — the original
/// complaint was that the effective number was undiscoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobsSource {
    SoldrJobsEnv,
    Config,
    ZccacheCompatEnv,
    Default,
}

impl JobsSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::SoldrJobsEnv => "SOLDR_JOBS",
            Self::Config => "config.toml [jobs].max_parallel_compiles",
            Self::ZccacheCompatEnv => "ZCCACHE_MAX_PARALLEL_COMPILES (compat)",
            Self::Default => "default",
        }
    }
}

/// The resolved limit plus where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedJobs {
    pub jobs: usize,
    pub source: JobsSource,
}

/// Default concurrency when nothing is configured.
///
/// Deliberately unchanged from what the vendored zccache semaphore
/// already used (`available_parallelism() - 1`), so this module is a
/// no-op for anyone who sets nothing. #1761 also floats a
/// physical-core-aware default (`min(logical - 1, physical + 2)`) to
/// stopped an 8C/16T host running 15 concurrent rustc. That is now
/// handled by [`default_compile_jobs_from`], which reads the real CPU
/// topology instead of assuming SMT.
pub fn default_compile_jobs() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    default_compile_jobs_from(logical, super::cpu_topology::physical_cores())
}

/// The default limit, as a pure function of the machine's shape.
///
/// Two rules, in order:
///
/// 1. Leave one logical CPU free (`logical - 1`), so a build cannot
///    starve the session that started it.
/// 2. When SMT is present, cap at `physical + 2`. rustc is
///    compute-bound, so the second thread of a core adds far less than
///    a second core does; letting all 16 threads of an 8-core host run
///    rustc saturates the machine for little throughput. The `+ 2`
///    keeps some benefit from SMT during the I/O-bound phases rather
///    than pinning the limit to the core count.
///
/// SMT is detected, not assumed: rule 2 applies only when `physical` is
/// genuinely below `logical`. A 16-core non-SMT host therefore keeps
/// all 15 slots — the objection that deferred this change originally.
/// When topology is unavailable (`None`) the behavior is unchanged from
/// before, which is the safe direction: no invented core count.
pub fn default_compile_jobs_from(logical: usize, physical: Option<usize>) -> usize {
    let headroom = logical.saturating_sub(1).max(1);
    match physical {
        Some(physical) if physical > 0 && physical < logical => {
            headroom.min(physical.saturating_add(2)).max(1)
        }
        _ => headroom,
    }
}

/// Resolve the compile-concurrency limit from an explicit set of
/// inputs. Pure, so the precedence is unit-testable without touching
/// the process environment.
pub fn resolve_compile_jobs_from(
    soldr_jobs_env: Option<&str>,
    config: Option<usize>,
    zccache_env: Option<&str>,
    default_jobs: usize,
) -> ResolvedJobs {
    if let Some(jobs) = soldr_jobs_env.and_then(parse_positive) {
        return ResolvedJobs {
            jobs,
            source: JobsSource::SoldrJobsEnv,
        };
    }
    if let Some(jobs) = config.filter(|value| *value > 0) {
        return ResolvedJobs {
            jobs,
            source: JobsSource::Config,
        };
    }
    if let Some(jobs) = zccache_env.and_then(parse_positive) {
        return ResolvedJobs {
            jobs,
            source: JobsSource::ZccacheCompatEnv,
        };
    }
    ResolvedJobs {
        jobs: default_jobs.max(1),
        source: JobsSource::Default,
    }
}

/// [`resolve_compile_jobs_from`] against the live environment and the
/// caller's parsed config.
pub fn resolve_compile_jobs(config: Option<usize>) -> ResolvedJobs {
    let soldr = std::env::var(SOLDR_JOBS_ENV_VAR).ok();
    let zccache = std::env::var(ZCCACHE_MAX_PARALLEL_COMPILES_ENV_VAR).ok();
    resolve_compile_jobs_from(
        soldr.as_deref(),
        config,
        zccache.as_deref(),
        default_compile_jobs(),
    )
}

/// Parse a positive limit. Empty, non-numeric, and `0` values fall
/// through to the next tier rather than erroring: an unset-looking or
/// nonsensical value should not be able to wedge every build, and `0`
/// would otherwise mean "admit nothing".
fn parse_positive(raw: &str) -> Option<usize> {
    raw.trim().parse::<usize>().ok().filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#1761's final acceptance criterion, stated as its own
    // numbers: "on an 8C/16T host the interactive default lands in the
    // 9-12 range rather than 15."
    #[test]
    fn an_8c_16t_host_no_longer_defaults_to_15() {
        let jobs = default_compile_jobs_from(16, Some(8));
        assert_eq!(jobs, 10, "8C/16T must land in the 9-12 range, not 15");
    }

    #[test]
    fn a_non_smt_host_keeps_every_slot() {
        // The objection that deferred this change: a 16-core machine
        // with no SMT must not be capped at 10 for an SMT discount it
        // does not need.
        assert_eq!(default_compile_jobs_from(16, Some(16)), 15);
    }

    #[test]
    fn unknown_topology_behaves_exactly_as_before() {
        for logical in [1usize, 2, 4, 16, 64] {
            assert_eq!(
                default_compile_jobs_from(logical, None),
                logical.saturating_sub(1).max(1),
                "an unreadable topology must not change the limit"
            );
        }
    }

    #[test]
    fn the_default_is_always_at_least_one() {
        for logical in [0usize, 1, 2] {
            for physical in [None, Some(0), Some(1), Some(usize::MAX)] {
                assert!(
                    default_compile_jobs_from(logical, physical) >= 1,
                    "logical={logical} physical={physical:?} produced a \
                     limit below 1, which would admit no compiles at all"
                );
            }
        }
    }

    #[test]
    fn smaller_hosts_scale_down_too() {
        assert_eq!(default_compile_jobs_from(8, Some(4)), 6);
        assert_eq!(default_compile_jobs_from(4, Some(2)), 3);
        // physical + 2 exceeds the headroom here, so the headroom wins
        // and the two rules never fight.
        assert_eq!(default_compile_jobs_from(2, Some(1)), 1);
    }

    #[test]
    fn soldr_jobs_env_wins_over_everything() {
        let resolved = resolve_compile_jobs_from(Some("3"), Some(9), Some("11"), 16);
        assert_eq!(resolved.jobs, 3);
        assert_eq!(resolved.source, JobsSource::SoldrJobsEnv);
    }

    #[test]
    fn config_wins_over_the_zccache_compat_var() {
        let resolved = resolve_compile_jobs_from(None, Some(9), Some("11"), 16);
        assert_eq!(resolved.jobs, 9);
        assert_eq!(resolved.source, JobsSource::Config);
    }

    #[test]
    fn zccache_var_still_works_for_existing_setups() {
        // The pre-#1761 knob must keep working, or upgrading soldr
        // silently changes the concurrency on machines already tuned.
        let resolved = resolve_compile_jobs_from(None, None, Some("11"), 16);
        assert_eq!(resolved.jobs, 11);
        assert_eq!(resolved.source, JobsSource::ZccacheCompatEnv);
    }

    #[test]
    fn falls_back_to_the_default_when_nothing_is_set() {
        let resolved = resolve_compile_jobs_from(None, None, None, 16);
        assert_eq!(resolved.jobs, 16);
        assert_eq!(resolved.source, JobsSource::Default);
    }

    #[test]
    fn unparseable_and_zero_values_fall_through_rather_than_wedging() {
        // A limit of 0 admits nothing, so every compile would block
        // forever. Falling through beats deadlocking the build.
        for bad in ["", "   ", "0", "-1", "many", "3.5"] {
            let resolved = resolve_compile_jobs_from(Some(bad), None, None, 16);
            assert_eq!(
                resolved.source,
                JobsSource::Default,
                "SOLDR_JOBS={bad:?} must fall through",
            );
            assert_eq!(resolved.jobs, 16);
        }
        let zero_config = resolve_compile_jobs_from(None, Some(0), None, 16);
        assert_eq!(zero_config.source, JobsSource::Default);
    }

    #[test]
    fn whitespace_is_tolerated() {
        let resolved = resolve_compile_jobs_from(Some(" 6 "), None, None, 16);
        assert_eq!(resolved.jobs, 6);
    }

    #[test]
    fn resolved_limit_is_never_zero() {
        let resolved = resolve_compile_jobs_from(None, None, None, 0);
        assert_eq!(resolved.jobs, 1);
    }

    #[test]
    fn default_is_at_least_one_and_below_logical_parallelism() {
        let jobs = default_compile_jobs();
        assert!(jobs >= 1);
        if let Ok(logical) = std::thread::available_parallelism() {
            assert!(
                jobs <= logical.get(),
                "default {jobs} must not exceed logical parallelism {logical:?}",
            );
        }
    }
}
