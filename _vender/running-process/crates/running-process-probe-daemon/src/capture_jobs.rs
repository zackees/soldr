//! Async cooperative-capture job coordination (#637).
//!
//! Operators enqueue work; registered targets lease it on their next
//! heartbeat. A connection may hold one lease at a time, so a subsequent
//! `CaptureReply` is correlated by the OS-authenticated connection rather than
//! by trusting an artifact filename or adding another wire identifier.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use running_process_probe::probe_diag::v1::{
    CaptureReply, CaptureStackRequest, JobStatus, ProcessKey as WireProcessKey,
};

use crate::registry::ProcessKey;

const DEFAULT_JOB_DEADLINE_MS: u64 = 30_000;
const MAX_TOTAL_JOBS: usize = 1_024;
const MAX_ACTIVE_JOBS_PER_TARGET: usize = 32;
const TERMINAL_TTL_MS: u64 = 5 * 60_000;

/// An uploaded capture whose lease was authenticated by its connection.
#[derive(Debug)]
pub struct CaptureUpload {
    /// Daemon-assigned job identifier.
    pub job_id: String,
    /// Target's result.
    pub reply: CaptureReply,
    /// Absolute deadline shared by capture and all worker passes.
    pub deadline_unix_ms: u64,
}

#[derive(Debug)]
enum State {
    Pending,
    Running {
        conn_id: u64,
    },
    Complete {
        artifact_path: String,
        finished_unix_ms: u64,
    },
    Failed {
        error: i32,
        detail: String,
        finished_unix_ms: u64,
        conn_id: Option<u64>,
    },
}

#[derive(Debug)]
struct Job {
    id: String,
    target: ProcessKey,
    request: CaptureStackRequest,
    deadline_unix_ms: u64,
    state: State,
}

#[derive(Debug, Default)]
struct Inner {
    jobs: HashMap<String, Job>,
    queue: VecDeque<String>,
}

/// Thread-safe capture queue shared by every daemon connection.
#[derive(Debug, Default)]
pub struct CaptureJobs {
    inner: Mutex<Inner>,
}

impl CaptureJobs {
    /// Enqueue an eligible capture and return its asynchronous receipt.
    pub fn enqueue(
        &self,
        target: ProcessKey,
        max_depth: u32,
        thread_filter: u32,
        deadline_unix_ms: u64,
    ) -> Result<CaptureReply, &'static str> {
        let started_unix_ms = unix_millis();
        let deadline_unix_ms = if deadline_unix_ms == 0 {
            started_unix_ms.saturating_add(DEFAULT_JOB_DEADLINE_MS)
        } else {
            deadline_unix_ms
        };
        let mut inner = self.inner.lock().expect("capture jobs poisoned");
        maintain(&mut inner, started_unix_ms);
        if inner.jobs.len() >= MAX_TOTAL_JOBS {
            return Err("capture job capacity reached");
        }
        let active_for_target = inner
            .jobs
            .values()
            .filter(|job| {
                job.target == target && matches!(job.state, State::Pending | State::Running { .. })
            })
            .count();
        if active_for_target >= MAX_ACTIVE_JOBS_PER_TARGET {
            return Err("target has too many active capture jobs");
        }

        let mut id = new_job_id();
        while inner.jobs.contains_key(&id) {
            id = new_job_id();
        }
        let request = CaptureStackRequest {
            key: Some(to_wire_key(&target)),
            max_depth,
            thread_filter,
            ..Default::default()
        };
        let job = Job {
            id: id.clone(),
            target,
            request,
            deadline_unix_ms,
            state: State::Pending,
        };
        inner.queue.push_back(id.clone());
        inner.jobs.insert(id.clone(), job);
        Ok(CaptureReply {
            job_id: id,
            started_unix_ms,
            ..Default::default()
        })
    }

    /// Lease the next capture for `target`, if this connection has none.
    pub fn lease(&self, target: &ProcessKey, conn_id: u64) -> Option<CaptureStackRequest> {
        let mut inner = self.inner.lock().expect("capture jobs poisoned");
        maintain(&mut inner, unix_millis());
        if inner
            .jobs
            .values()
            .any(|job| {
                matches!(job.state, State::Running { conn_id: active } if active == conn_id)
                    || matches!(job.state, State::Failed { conn_id: Some(active), .. } if active == conn_id)
            })
        {
            return None;
        }

        let position = inner.queue.iter().position(|id| {
            inner
                .jobs
                .get(id)
                .is_some_and(|job| job.target == *target && matches!(job.state, State::Pending))
        })?;
        let id = inner.queue.remove(position)?;
        let job = inner.jobs.get_mut(&id)?;
        job.state = State::Running { conn_id };
        Some(job.request.clone())
    }

    /// Authenticate a target upload against the single lease on `conn_id`.
    pub fn accept_upload(
        &self,
        conn_id: u64,
        reply: CaptureReply,
    ) -> Result<CaptureUpload, &'static str> {
        let mut inner = self.inner.lock().expect("capture jobs poisoned");
        maintain(&mut inner, unix_millis());
        let Some(job) = inner.jobs.values_mut().find(|job| {
            matches!(job.state, State::Running { conn_id: active } if active == conn_id)
                || matches!(job.state, State::Failed { conn_id: Some(active), .. } if active == conn_id)
        }) else {
            return Err("connection has no leased capture");
        };
        let job_id = job.id.clone();
        if let State::Failed {
            conn_id: failed_conn,
            ..
        } = &mut job.state
        {
            // A deadline may have elapsed while the target was capturing.
            // Consume and ACK that one late result so a job-level timeout does
            // not masquerade as a dead transport.
            *failed_conn = None;
        } else if reply.error != 0 {
            job.state = State::Failed {
                error: reply.error,
                detail: reply.detail.clone(),
                finished_unix_ms: unix_millis(),
                conn_id: None,
            };
        }
        Ok(CaptureUpload {
            job_id,
            reply,
            deadline_unix_ms: job.deadline_unix_ms,
        })
    }

    /// Record successful off-process symbolization.
    pub fn complete(&self, job_id: &str, artifact_path: String) -> bool {
        let now = unix_millis();
        let mut inner = self.inner.lock().expect("capture jobs poisoned");
        expire(&mut inner, now);
        let Some(job) = inner.jobs.get_mut(job_id) else {
            return false;
        };
        if !matches!(job.state, State::Running { .. }) {
            return false;
        }
        job.state = State::Complete {
            artifact_path,
            finished_unix_ms: now,
        };
        true
    }

    /// Record capture or worker failure without affecting another job.
    pub fn fail(&self, job_id: &str, error: i32, detail: String) {
        if let Some(job) = self
            .inner
            .lock()
            .expect("capture jobs poisoned")
            .jobs
            .get_mut(job_id)
        {
            if matches!(job.state, State::Pending | State::Running { .. }) {
                job.state = State::Failed {
                    error,
                    detail,
                    finished_unix_ms: unix_millis(),
                    conn_id: None,
                };
            }
        }
    }

    /// Snapshot the public state of one job.
    pub fn status(&self, job_id: &str) -> Option<JobStatus> {
        let mut inner = self.inner.lock().expect("capture jobs poisoned");
        maintain(&mut inner, unix_millis());
        let job = inner.jobs.get(job_id)?;
        let (state, percent_complete, artifact_path, error, detail) = match &job.state {
            State::Pending => (1, 0, String::new(), 0, String::new()),
            State::Running { .. } => (2, 50, String::new(), 0, String::new()),
            State::Complete { artifact_path, .. } => {
                (3, 100, artifact_path.clone(), 0, String::new())
            }
            State::Failed { error, detail, .. } => (4, 100, String::new(), *error, detail.clone()),
        };
        Some(JobStatus {
            job_id: job.id.clone(),
            state,
            percent_complete,
            artifact_path,
            error,
            detail,
        })
    }
}

fn maintain(inner: &mut Inner, now: u64) {
    expire(inner, now);
    let cutoff = now.saturating_sub(TERMINAL_TTL_MS);
    inner.jobs.retain(|_, job| {
        let keep = match job.state {
            State::Complete {
                finished_unix_ms, ..
            }
            | State::Failed {
                finished_unix_ms, ..
            } => finished_unix_ms > cutoff,
            State::Pending | State::Running { .. } => true,
        };
        if !keep {
            remove_report_artifacts(job);
        }
        keep
    });
    if inner.jobs.len() >= MAX_TOTAL_JOBS {
        let mut terminal: Vec<(String, u64)> = inner
            .jobs
            .iter()
            .filter_map(|(id, job)| match job.state {
                State::Complete {
                    finished_unix_ms, ..
                }
                | State::Failed {
                    finished_unix_ms, ..
                } => Some((id.clone(), finished_unix_ms)),
                State::Pending | State::Running { .. } => None,
            })
            .collect();
        terminal.sort_by_key(|(_, finished)| *finished);
        for (id, _) in terminal {
            if inner.jobs.len() < MAX_TOTAL_JOBS {
                break;
            }
            if let Some(job) = inner.jobs.remove(&id) {
                remove_report_artifacts(&job);
            }
        }
    }
    inner.queue.retain(|id| {
        inner
            .jobs
            .get(id)
            .is_some_and(|job| matches!(job.state, State::Pending))
    });
}

fn remove_report_artifacts(job: &Job) {
    let State::Complete { artifact_path, .. } = &job.state else {
        return;
    };
    let json = std::path::Path::new(artifact_path);
    let Some(file_name) = json.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Some(stem) = file_name
        .strip_prefix("rp-probe-report-")
        .and_then(|name| name.strip_suffix(".symbolized.json"))
    else {
        return;
    };
    let Some(parent) = json.parent() else {
        return;
    };
    let Ok(parent) = parent.canonicalize() else {
        return;
    };
    let Ok(expected) = std::env::temp_dir().canonicalize() else {
        return;
    };
    if parent != expected {
        return;
    }
    let json = parent.join(file_name);
    let text = parent.join(format!("rp-probe-report-{stem}.symbolized.txt"));
    let _ = std::fs::remove_file(json);
    let _ = std::fs::remove_file(text);
}

fn expire(inner: &mut Inner, now: u64) {
    for job in inner.jobs.values_mut() {
        if now >= job.deadline_unix_ms
            && matches!(job.state, State::Pending | State::Running { .. })
        {
            let conn_id = match &job.state {
                State::Running { conn_id } => Some(*conn_id),
                State::Pending | State::Complete { .. } | State::Failed { .. } => None,
            };
            job.state = State::Failed {
                // PROBE_ERROR_DEADLINE.
                error: 4,
                detail: "capture deadline elapsed".into(),
                finished_unix_ms: now,
                conn_id,
            };
        }
    }
}

fn to_wire_key(key: &ProcessKey) -> WireProcessKey {
    WireProcessKey {
        pid: u64::from(key.pid),
        start_time: Some(key.started_at_unix_ms),
        boot_id: Some(key.boot_id.clone()),
    }
}

fn new_job_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = unix_millis().to_le_bytes();
        bytes[..fallback.len()].copy_from_slice(&fallback);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ProcessKey {
        ProcessKey {
            pid: 7,
            started_at_unix_ms: 11,
            boot_id: "boot".into(),
        }
    }

    #[test]
    fn queue_lease_upload_complete_is_one_correlated_job() {
        let jobs = CaptureJobs::default();
        let receipt = jobs.enqueue(key(), 64, 0, 0).expect("enqueued");
        let leased = jobs.lease(&key(), 42).expect("leased");
        assert_eq!(leased.max_depth, 64);
        assert!(
            jobs.lease(&key(), 42).is_none(),
            "one connection must not hold two ambiguous leases"
        );
        let upload = jobs
            .accept_upload(
                42,
                CaptureReply {
                    artifact_path: "raw.json".into(),
                    ..Default::default()
                },
            )
            .expect("authenticated upload");
        assert_eq!(upload.job_id, receipt.job_id);
        assert!(jobs.complete(&upload.job_id, "report.json".into()));
        let status = jobs.status(&receipt.job_id).expect("status");
        assert_eq!(status.state, 3);
        assert_eq!(status.artifact_path, "report.json");
    }

    #[test]
    fn another_connection_cannot_complete_a_lease() {
        let jobs = CaptureJobs::default();
        jobs.enqueue(key(), 64, 0, 0).expect("enqueued");
        jobs.lease(&key(), 42).expect("leased");
        assert_eq!(
            jobs.accept_upload(99, CaptureReply::default()).unwrap_err(),
            "connection has no leased capture"
        );
    }

    #[test]
    fn completion_cannot_overwrite_an_expired_job() {
        let jobs = CaptureJobs::default();
        let receipt = jobs.enqueue(key(), 64, 0, u64::MAX).expect("enqueued");
        jobs.lease(&key(), 42).expect("leased");
        {
            let mut inner = jobs.inner.lock().unwrap();
            inner
                .jobs
                .get_mut(&receipt.job_id)
                .unwrap()
                .deadline_unix_ms = 0;
        }
        assert!(!jobs.complete(&receipt.job_id, "late.json".into()));
        assert_eq!(jobs.status(&receipt.job_id).unwrap().state, 4);
    }

    #[test]
    fn one_late_result_for_an_expired_lease_is_consumed_without_transport_failure() {
        let jobs = CaptureJobs::default();
        let receipt = jobs.enqueue(key(), 64, 0, u64::MAX).expect("enqueued");
        jobs.lease(&key(), 42).expect("leased");
        {
            let mut inner = jobs.inner.lock().unwrap();
            inner
                .jobs
                .get_mut(&receipt.job_id)
                .unwrap()
                .deadline_unix_ms = 0;
        }
        assert_eq!(jobs.status(&receipt.job_id).unwrap().state, 4);
        jobs.enqueue(key(), 64, 0, 0).expect("next job queued");
        assert!(
            jobs.lease(&key(), 42).is_none(),
            "a late reply remains correlated until it is consumed"
        );
        let upload = jobs
            .accept_upload(42, CaptureReply::default())
            .expect("late result is acknowledged once");
        assert_eq!(upload.job_id, receipt.job_id);
        assert_eq!(
            jobs.accept_upload(42, CaptureReply::default()).unwrap_err(),
            "connection has no leased capture"
        );
        assert!(
            jobs.lease(&key(), 42).is_some(),
            "the next job may lease after the late reply is consumed"
        );
    }

    #[test]
    fn per_target_active_admission_is_bounded() {
        let jobs = CaptureJobs::default();
        for _ in 0..MAX_ACTIVE_JOBS_PER_TARGET {
            jobs.enqueue(key(), 64, 0, 0).expect("within cap");
        }
        assert_eq!(
            jobs.enqueue(key(), 64, 0, 0).unwrap_err(),
            "target has too many active capture jobs"
        );
    }

    #[test]
    fn total_admission_is_bounded() {
        let jobs = CaptureJobs::default();
        for pid in 1..=MAX_TOTAL_JOBS {
            let target = ProcessKey {
                pid: pid as u32,
                started_at_unix_ms: 11,
                boot_id: "boot".into(),
            };
            jobs.enqueue(target, 64, 0, 0).expect("within cap");
        }
        assert_eq!(
            jobs.enqueue(key(), 64, 0, 0).unwrap_err(),
            "capture job capacity reached"
        );
    }

    #[test]
    fn terminal_jobs_expire_after_the_retention_window() {
        let jobs = CaptureJobs::default();
        let receipt = jobs.enqueue(key(), 64, 0, 0).expect("enqueued");
        jobs.lease(&key(), 42).expect("leased");
        let json = std::env::temp_dir().join(format!(
            "rp-probe-report-{}.symbolized.json",
            receipt.job_id
        ));
        let text =
            std::env::temp_dir().join(format!("rp-probe-report-{}.symbolized.txt", receipt.job_id));
        std::fs::write(&json, b"json").unwrap();
        std::fs::write(&text, b"text").unwrap();
        assert!(jobs.complete(&receipt.job_id, json.to_string_lossy().into_owned()));
        {
            let mut inner = jobs.inner.lock().unwrap();
            let State::Complete {
                finished_unix_ms, ..
            } = &mut inner.jobs.get_mut(&receipt.job_id).unwrap().state
            else {
                panic!("expected complete");
            };
            *finished_unix_ms = 1;
            maintain(&mut inner, TERMINAL_TTL_MS + 2);
        }
        assert!(jobs.status(&receipt.job_id).is_none());
        assert!(!json.exists());
        assert!(!text.exists());
    }
}
