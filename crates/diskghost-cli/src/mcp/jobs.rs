//! Background jobs and the one-writer rule.
//!
//! Every job writes the index (scans, `dupes_report`, `plan_dedup`), and the
//! index has one writer at a time, so jobs take turns: the first runs, the
//! rest are `queued` in order. A queued or running job can be cancelled
//! through its [`Progress`] handle. Jobs live in this process only — when the
//! server ends, they end; a local scan holds a single transaction, so a
//! killed one records nothing.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use diskghost_core::{phase, Progress};
use serde_json::{json, Value};

use super::ToolError;

/// What a job produces: the structured result, or why it failed.
pub(crate) type JobResult = Result<Value, ToolError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

struct Job {
    id: String,
    kind: &'static str,
    target: String,
    state: State,
    progress: Progress,
    created: Instant,
    started: Option<Instant>,
    finished: Option<Instant>,
    result: Option<Value>,
    error: Option<ToolError>,
}

#[derive(Default)]
struct Inner {
    jobs: Vec<Job>,
    /// Indices of jobs waiting for the writer turn, oldest first.
    queue: VecDeque<usize>,
    /// The job holding the writer turn.
    active: Option<usize>,
}

#[derive(Default)]
pub(crate) struct Jobs {
    inner: Mutex<Inner>,
    changed: Condvar,
}

fn phase_name(progress: &Progress) -> &'static str {
    match progress.phase() {
        phase::SAMPLE => "sample",
        phase::HASH => "hash",
        _ => "walk",
    }
}

impl Jobs {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue `work` as a writer job; it starts when every earlier job has
    /// finished. `hold` idles inside the turn (test hook, normally zero).
    pub fn submit(
        self: &Arc<Jobs>,
        kind: &'static str,
        target: String,
        hold: Duration,
        work: Box<dyn FnOnce(&Progress) -> JobResult + Send>,
    ) -> String {
        let progress = Progress::default();
        let (idx, id) = {
            let mut g = self.lock();
            let idx = g.jobs.len();
            let id = format!("job-{}", idx + 1);
            g.jobs.push(Job {
                id: id.clone(),
                kind,
                target,
                state: State::Queued,
                progress: progress.clone(),
                created: Instant::now(),
                started: None,
                finished: None,
                result: None,
                error: None,
            });
            g.queue.push_back(idx);
            (idx, id)
        };
        let jobs = self.clone();
        std::thread::spawn(move || jobs.drive(idx, &progress, hold, work));
        id
    }

    fn drive(
        &self,
        idx: usize,
        progress: &Progress,
        hold: Duration,
        work: Box<dyn FnOnce(&Progress) -> JobResult + Send>,
    ) {
        // Wait for the writer turn; a cancel while queued ends the job there.
        {
            let mut g = self.lock();
            loop {
                if progress.cancelled() {
                    g.queue.retain(|&i| i != idx);
                    let job = &mut g.jobs[idx];
                    job.state = State::Cancelled;
                    job.finished = Some(Instant::now());
                    self.changed.notify_all();
                    return;
                }
                if g.active.is_none() && g.queue.front() == Some(&idx) {
                    g.queue.pop_front();
                    g.active = Some(idx);
                    let job = &mut g.jobs[idx];
                    job.state = State::Running;
                    job.started = Some(Instant::now());
                    self.changed.notify_all();
                    break;
                }
                g = self
                    .changed
                    .wait_timeout(g, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        let end = Instant::now() + hold;
        while Instant::now() < end && !progress.cancelled() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = if progress.cancelled() {
            None
        } else {
            Some(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(progress)))
                    .unwrap_or_else(|_| Err(ToolError::new("internal", "the job panicked"))),
            )
        };
        let mut g = self.lock();
        let job = &mut g.jobs[idx];
        job.finished = Some(Instant::now());
        match out {
            None => job.state = State::Cancelled,
            Some(Ok(v)) => {
                job.state = if progress.cancelled() {
                    State::Cancelled
                } else {
                    State::Done
                };
                job.result = Some(v);
            }
            Some(Err(e)) if e.code == "cancelled" => job.state = State::Cancelled,
            Some(Err(e)) => {
                job.state = State::Failed;
                job.error = Some(e);
            }
        }
        g.active = None;
        self.changed.notify_all();
    }

    fn snapshot(job: &Job) -> Value {
        let now = Instant::now();
        let elapsed = match (job.started, job.finished) {
            (Some(s), Some(f)) => f.duration_since(s),
            (Some(s), None) => now.duration_since(s),
            _ => Duration::ZERO,
        };
        let queued = job.started.unwrap_or(now).duration_since(job.created);
        let mut v = json!({
            "job_id": job.id,
            "kind": job.kind,
            "target": job.target,
            "state": job.state.name(),
            "phase": if job.state == State::Queued { "queued" } else { phase_name(&job.progress) },
            "files": job.progress.files(),
            "bytes": job.progress.bytes(),
            "total_files": job.progress.total_files(),
            "total_bytes": job.progress.total_bytes(),
            "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            "queued_ms": u64::try_from(queued.as_millis()).unwrap_or(u64::MAX),
        });
        if let Some(r) = &job.result {
            v["result"] = r.clone();
        }
        if let Some(e) = &job.error {
            v["error"] = e.to_json();
        }
        v
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        let g = self.lock();
        g.jobs.iter().find(|j| j.id == id).map(Jobs::snapshot)
    }

    pub fn list(&self) -> Vec<Value> {
        self.lock().jobs.iter().map(Jobs::snapshot).collect()
    }

    /// The job's outcome once it has finished (state, result, error), or
    /// `None` after `timeout` — or for an unknown id.
    pub fn wait_finished(
        &self,
        id: &str,
        timeout: Duration,
    ) -> Option<(State, Option<Value>, Option<ToolError>)> {
        let end = Instant::now() + timeout;
        let mut g = self.lock();
        loop {
            let job = g.jobs.iter().find(|j| j.id == id)?;
            if job.state.finished() {
                return Some((job.state, job.result.clone(), job.error.clone()));
            }
            let left = end.checked_duration_since(Instant::now())?;
            g = self
                .changed
                .wait_timeout(g, left.min(Duration::from_millis(100)))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Ask a job to stop. `None` for an unknown id; otherwise whether the
    /// request could still matter (the job had not finished).
    pub fn cancel(&self, id: &str) -> Option<bool> {
        let g = self.lock();
        let job = g.jobs.iter().find(|j| j.id == id)?;
        let live = !job.state.finished();
        if live {
            job.progress.cancel();
        }
        drop(g);
        self.changed.notify_all();
        Some(live)
    }

    pub fn cancel_all(&self) {
        for job in &self.lock().jobs {
            job.progress.cancel();
        }
        self.changed.notify_all();
    }

    /// No job queued or running.
    pub fn all_idle(&self) -> bool {
        self.lock().jobs.iter().all(|j| j.state.finished())
    }

    /// The id of the job holding the writer turn, if any.
    pub fn writer_active(&self) -> Option<String> {
        let g = self.lock();
        g.active.map(|i| g.jobs[i].id.clone())
    }
}
