//! Per-phase startup timing emitted to stderr when
//! `SOLDR_PROFILE_STARTUP=1` is set. The hot user-visible path —
//! `RUSTC_WRAPPER=soldr` invoked by cargo 100+ times per build —
//! pays ~47 ms per invocation post-zccache-1.9.0 (issue #440). To
//! pick which lever (process startup, ensure_dirs, registry upsert,
//! IPC connect, …) actually matters, the wrapper code marks each
//! phase boundary; this module prints the deltas at exit.
//!
//! Zero cost when the env var is unset: `WrapperProfile::new()`
//! returns a `disabled` instance whose `mark()` and `finish()`
//! short-circuit immediately. No allocation, no `Instant::now()` —
//! we don't want the diagnostic itself to skew measurements.
//!
//! The output goes to stderr (where cargo's own progress lines
//! live) and is prefixed with `soldr-profile:` so downstream
//! scrapers can grep for it without false matches against ordinary
//! soldr log lines:
//!
//! ```text
//! soldr-profile: wrapper invocation breakdown (process_pid=12345)
//! soldr-profile:                 main_entry  +    87 µs
//! soldr-profile:           args_collected  +    21 µs
//! soldr-profile:         relocate_checked  +    18 µs
//! soldr-profile:     wrapper_dispatch_in  +     3 µs
//! soldr-profile:    record_target_dir_done  +   412 µs
//! soldr-profile:        before_zccache_exec  +    72 µs
//! soldr-profile:                       total       613 µs
//! ```
//!
//! See issue #440 acceptance criterion #1 — "A measurement of
//! soldr's per-invocation overhead on Linux, broken into: process
//! startup, arg parse, env read, IPC connect, request marshal,
//! response decode, exit."

use std::time::Instant;

/// Env var that enables per-phase timing output. Any non-empty value
/// turns it on. Soldr-internal callers ignore the variable's content
/// (only presence matters); future versions may key off specific
/// values (`per-invocation`, `summary-only`) for selective output.
pub(crate) const SOLDR_PROFILE_STARTUP_ENV_VAR: &str = "SOLDR_PROFILE_STARTUP";

/// Accumulates per-phase wall-clock marks. When the env var is set,
/// the wrapper hot path calls `mark("phase_name")` at each boundary
/// and `finish()` right before the exec'/exit. When unset, every
/// method is a no-op so we don't perturb the measurement of the
/// non-instrumented path.
pub struct WrapperProfile {
    enabled: bool,
    start: Option<Instant>,
    /// Pre-allocated to avoid `Vec::push` reallocs in the hot path
    /// when enabled. 16 phases is comfortably more than any
    /// realistic wrapper invocation marks today.
    phases: Vec<(&'static str, Instant)>,
}

impl Default for WrapperProfile {
    fn default() -> Self {
        Self::new()
    }
}

impl WrapperProfile {
    /// Construct a profile guard. Captures `Instant::now()` only
    /// when the env var is set, so the disabled path adds at most
    /// one cheap branch + one `var_os` syscall.
    pub fn new() -> Self {
        let enabled = std::env::var_os(SOLDR_PROFILE_STARTUP_ENV_VAR)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        Self {
            enabled,
            start: enabled.then(Instant::now),
            phases: if enabled {
                Vec::with_capacity(16)
            } else {
                Vec::new()
            },
        }
    }

    /// True iff profiling is on. Lets callers skip expensive
    /// instrumentation-only work (sub-phase splits) entirely.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record a phase boundary. `name` is the boundary that JUST
    /// completed — e.g. `mark("ensure_dirs_done")` is called right
    /// after `paths.ensure_dirs()` returns. The reported delta in
    /// the rendered output is from the previous mark (or `start`
    /// for the first one).
    ///
    /// No-op when profiling is disabled.
    pub fn mark(&mut self, name: &'static str) {
        if self.enabled {
            self.phases.push((name, Instant::now()));
        }
    }

    /// Render the accumulated marks to stderr as a sequence of
    /// `+delta µs` lines plus a `total` line. Should be called
    /// immediately before the wrapper's terminal `exec()` (or
    /// `std::process::exit`) so the measurement reflects all
    /// in-process work.
    ///
    /// On Unix `exec()` replaces the process so this is the LAST
    /// soldr-side code that runs — calling it after the spawn for
    /// the eventual zccache process is too late.
    pub fn finish(self, last_phase: &'static str) {
        self.finish_labeled("wrapper invocation", last_phase);
    }

    /// Same as [`finish`](Self::finish) but names the scope being
    /// measured. The wrapper hot path is not the only place worth
    /// attributing: `soldr cargo ...` pays a fixed front-door cost
    /// before Cargo is ever spawned, and on a fully warm no-op that
    /// cost — not compilation — dominates the wall clock (#1843).
    /// Reusing this type keeps one output format for both scopes.
    pub fn finish_labeled(self, scope: &str, last_phase: &'static str) {
        if !self.enabled {
            return;
        }
        let Some(start) = self.start else {
            return;
        };

        let mut buf = String::with_capacity(64 * self.phases.len() + 128);
        buf.push_str(&format!(
            "soldr-profile: {scope} breakdown (pid={})\n",
            std::process::id()
        ));

        let mut prev = start;
        for (name, at) in &self.phases {
            let delta = at.duration_since(prev);
            buf.push_str(&format!(
                "soldr-profile: {:>26}  +{:>6} µs\n",
                name,
                delta.as_micros(),
            ));
            prev = *at;
        }

        // The last_phase delta is `now - prev`. We record `now`
        // here so callers don't have to thread a final `mark()`
        // through every exit path.
        let now = Instant::now();
        let last_delta = now.duration_since(prev);
        buf.push_str(&format!(
            "soldr-profile: {:>26}  +{:>6} µs\n",
            last_phase,
            last_delta.as_micros(),
        ));

        let total = now.duration_since(start);
        buf.push_str(&format!(
            "soldr-profile: {:>26}   {:>6} µs\n",
            "total",
            total.as_micros(),
        ));

        // Single `write_all` to avoid interleaving with parallel
        // wrapper invocations from cargo's `--jobs N` worker pool.
        use std::io::Write;
        let _ = std::io::stderr().lock().write_all(buf.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test-wide lock that serializes the env-var mutations below.
    /// Without it, parallel test execution can clobber
    /// `SOLDR_PROFILE_STARTUP` between `EnvGuard::set` and the
    /// next-line `WrapperProfile::new()` read — caught by CI Linux
    /// x64 where `mark_records_phase_when_enabled` flaked with
    /// `phases.len() == 0` instead of `2`. Reproes locally too if
    /// you re-run the suite a few times.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Local RAII guard that sets / restores the profile env var
    /// AND holds `ENV_LOCK` for the lifetime of the guard.
    struct EnvGuard {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _guard: guard,
            }
        }

        fn unset(key: &'static str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self {
                key,
                prior,
                _guard: guard,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn disabled_when_env_var_unset() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let profile = WrapperProfile::new();
        assert!(!profile.is_enabled());
        assert!(profile.start.is_none());
    }

    #[test]
    fn disabled_when_env_var_empty() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "");
        let profile = WrapperProfile::new();
        assert!(
            !profile.is_enabled(),
            "empty value should not enable profiling",
        );
    }

    #[test]
    fn enabled_when_env_var_set_to_anything_non_empty() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "1");
        let profile = WrapperProfile::new();
        assert!(profile.is_enabled());
        assert!(profile.start.is_some());
    }

    #[test]
    fn mark_records_phase_when_enabled() {
        let _g = EnvGuard::set(SOLDR_PROFILE_STARTUP_ENV_VAR, "1");
        let mut profile = WrapperProfile::new();
        profile.mark("first_phase");
        profile.mark("second_phase");
        assert_eq!(profile.phases.len(), 2);
        assert_eq!(profile.phases[0].0, "first_phase");
        assert_eq!(profile.phases[1].0, "second_phase");
        // Monotonic — second mark must be at or after the first.
        assert!(profile.phases[1].1 >= profile.phases[0].1);
    }

    #[test]
    fn mark_is_no_op_when_disabled() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let mut profile = WrapperProfile::new();
        for i in 0..1000 {
            profile.mark(if i % 2 == 0 { "even" } else { "odd" });
        }
        assert!(
            profile.phases.is_empty(),
            "disabled profile must not accumulate marks",
        );
    }

    #[test]
    fn finish_is_no_op_when_disabled() {
        let _g = EnvGuard::unset(SOLDR_PROFILE_STARTUP_ENV_VAR);
        let profile = WrapperProfile::new();
        // No panic, no stderr noise — this is the dominant production
        // path so we lock in that it stays cheap.
        profile.finish("would_have_been_total");
    }
}
