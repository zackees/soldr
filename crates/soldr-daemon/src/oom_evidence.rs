//! Whether the kernel actually OOM-killed anything in this cgroup.
//!
//! soldr#2878 / soldr#2781: when a compiler dies to a signal, soldr says the
//! kill "can indicate an OOM/resource-limit kill" and tells the reader to
//! "inspect the host's memory-pressure counters". Soldr is running on that
//! host. The counters are two file reads away, and every triage of this so far
//! has stopped at the hedge.
//!
//! Measured on a 4-core / 7.9 GiB Linux container at `SOLDR_JOBS=8`: a cold
//! `soldr cargo check -p soldr-cli` compiled 461 units, then died to a signal
//! on `soldr_daemon` with that message -- while `memory.events` reported
//! `oom_kill 0`, peak cgroup usage was 2.2 GiB, and `MemAvailable` never fell
//! below 4.2 GiB. The message named the most likely cause and was wrong.
//!
//! The evidence is asymmetric, and the wording downstream reflects that:
//!
//! * **Zero is decisive.** `memory.events` counts every process in the cgroup
//!   killed by *any* OOM killer over the cgroup's whole lifetime, so a zero
//!   means no OOM kill has ever happened here -- it rules memory out.
//! * **Non-zero is suggestive only.** The counter is cumulative and never
//!   resets, so a positive value may belong to an earlier build. It raises
//!   the hypothesis; it does not confirm it for *this* compile.
//! * **Unreadable says nothing.** A missing file (cgroup v1, a non-Linux host,
//!   a restricted mount) must not read as "no kill" -- a false exoneration is
//!   worse than the hedge it would replace.
//!
//! No `#[cfg]`: the paths simply do not exist off Linux, so the reader returns
//! `Unknown` there by the same path it uses for a restricted mount. That keeps
//! this inside the platform-cfg boundary rule and means the Windows and macOS
//! builds exercise the same code.

use std::path::Path;

/// What the cgroup can tell us about an OOM kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OomEvidence {
    /// The counters are readable and have never recorded a kill here.
    NoKillRecorded,
    /// The counters recorded `n` kills at some point in this cgroup's life.
    KillsRecorded(u64),
    /// The counters could not be read, so there is nothing to say.
    Unknown,
}

impl OomEvidence {
    /// One sentence for a compiler's stderr, or nothing.
    ///
    /// `Unknown` renders as an empty string on purpose: appending "could not
    /// read the counters" to a build failure adds noise on every non-Linux
    /// host without helping anyone.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::NoKillRecorded => concat!(
                "soldr: the kernel has recorded no OOM kill in this cgroup ",
                "(memory.events oom_kill=0), so this was not the memory limit ",
                "-- lowering the job count is unlikely to help, and the cause ",
                "is elsewhere (soldr#2878).\n"
            )
            .to_string(),
            Self::KillsRecorded(count) => format!(
                concat!(
                    "soldr: the kernel has OOM-killed {count} process(es) in ",
                    "this cgroup (memory.events oom_kill), which supports a ",
                    "memory kill here. The counter is cumulative for the ",
                    "cgroup's lifetime, so it is evidence rather than proof ",
                    "for this compile (soldr#2878).\n"
                ),
                count = count
            ),
            Self::Unknown => String::new(),
        }
    }
}

/// Read the OOM counters under `cgroup_root`.
pub(crate) fn read_at(cgroup_root: &Path) -> OomEvidence {
    let Ok(raw) = std::fs::read_to_string(cgroup_root.join("memory.events")) else {
        return OomEvidence::Unknown;
    };
    match total_oom_kills(&raw) {
        Some(0) => OomEvidence::NoKillRecorded,
        Some(count) => OomEvidence::KillsRecorded(count),
        // The file existed but carried neither key. Treat a shape we do not
        // recognise as unknown rather than as zero.
        None => OomEvidence::Unknown,
    }
}

/// Read the counters from the cgroup-v2 directory that owns this process.
pub(crate) fn read() -> OomEvidence {
    soldr_platform::host::resources::cgroup_v2_dir()
        .map(|path| read_at(&path))
        .unwrap_or(OomEvidence::Unknown)
}

/// `oom_kill` + `oom_group_kill` from a `memory.events` body.
///
/// Both are counted: a cgroup with `memory.oom.group` set reports its kills
/// under `oom_group_kill`, and a reader that looked only at `oom_kill` would
/// call that case "no OOM".
///
/// `None` when neither key is present -- an unrecognised shape is not a zero.
fn total_oom_kills(raw: &str) -> Option<u64> {
    let mut total: Option<u64> = None;
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        if key != "oom_kill" && key != "oom_group_kill" {
            continue;
        }
        let Ok(count) = value.parse::<u64>() else {
            continue;
        };
        total = Some(total.unwrap_or(0) + count);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cgroup_with(events: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("memory.events"), events).expect("write events");
        dir
    }

    #[test]
    fn a_zero_counter_rules_the_memory_limit_out() {
        let dir = cgroup_with("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n");
        assert_eq!(read_at(dir.path()), OomEvidence::NoKillRecorded);
        let text = read_at(dir.path()).describe();
        assert!(text.contains("no OOM kill"), "{text}");
        assert!(text.contains("unlikely to help"), "{text}");
    }

    #[test]
    fn group_kills_count_toward_the_total() {
        // A cgroup with `memory.oom.group` set reports kills here instead, so
        // reading only `oom_kill` would exonerate a real memory kill.
        let dir = cgroup_with("oom_kill 0\noom_group_kill 3\n");
        assert_eq!(read_at(dir.path()), OomEvidence::KillsRecorded(3));
    }

    #[test]
    fn both_kill_counters_are_summed() {
        let dir = cgroup_with("oom_kill 2\noom_group_kill 1\n");
        assert_eq!(read_at(dir.path()), OomEvidence::KillsRecorded(3));
    }

    #[test]
    fn a_recorded_kill_is_offered_as_evidence_not_proof() {
        // The counter never resets, so a positive value may belong to an
        // earlier build in the same container. Overclaiming here would just
        // move the hedge rather than remove it.
        let text = OomEvidence::KillsRecorded(1).describe();
        assert!(text.contains("evidence rather than proof"), "{text}");
        assert!(text.contains("cumulative"), "{text}");
    }

    #[test]
    fn a_missing_file_is_unknown_and_says_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_at(dir.path()), OomEvidence::Unknown);
        assert_eq!(read_at(dir.path()).describe(), "");
    }

    #[test]
    fn an_unrecognised_shape_is_unknown_rather_than_zero() {
        // cgroup v1, or a future rename. Reporting "no OOM kill" because the
        // keys were absent is a false exoneration, which is worse than the
        // hedge it would replace.
        let dir = cgroup_with("usage_in_bytes 1234\nfailcnt 7\n");
        assert_eq!(read_at(dir.path()), OomEvidence::Unknown);
    }

    #[test]
    fn a_malformed_count_does_not_derail_the_read() {
        let dir = cgroup_with("oom_kill notanumber\noom_group_kill 2\n");
        assert_eq!(read_at(dir.path()), OomEvidence::KillsRecorded(2));
    }

    #[test]
    fn the_host_read_never_panics() {
        // Runs on every platform in CI; off Linux it must take the Unknown
        // path rather than failing.
        let _ = read().describe();
    }
}
