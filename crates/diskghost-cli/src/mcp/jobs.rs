//! Background jobs and the one-writer rule.
//!
//! Every job writes the index (scans, `dupes_report`, `plan_dedup`), and the
//! index has one writer at a time, so jobs take turns: the first runs, the
//! rest are `queued` in order. A queued or running job can be cancelled
//! through its [`Progress`] handle.
//!
//! Jobs are shared through the job registry (`index-jobs.db`, see
//! [`diskghost_core::jobs`]): every submission, state transition and — on a
//! throttled beat, at most once per [`BEAT_MS`] — the live counters are
//! recorded, so any other diskghost process (a second MCP server, the GUI)
//! sees them. The in-memory queue stays authoritative for this process;
//! registry writes are best-effort and never change a job's outcome — a
//! local scan holds a single index transaction, so a killed one records
//! nothing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use diskghost_core::jobs::{now_ms, Beat, JobRegistry};
use diskghost_core::{phase, Progress};
use serde_json::{json, Value};

use super::ToolError;

/// How often the live jobs' counters and heartbeat reach the registry.
/// Time-based, from a dedicated thread — the scan's per-file hot loop never
/// touches the database.
const BEAT_MS: u64 = 1_500;

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
    /// The shared registry this process reports into (`index-jobs.db`);
    /// `None` when it could not be opened — jobs then live in this process
    /// only, as before.
    registry: Option<JobRegistry>,
    /// Stops the heartbeat thread (`Drop`, or `cancel_all` at shutdown).
    beat_stop: Arc<AtomicBool>,
}

fn phase_name(progress: &Progress) -> &'static str {
    match progress.phase() {
        phase::SAMPLE => "sample",
        phase::HASH => "hash",
        _ => "walk",
    }
}

impl Jobs {
    /// A `Jobs` that reports into `registry` (when given): its own beat
    /// thread refreshes heartbeats and the live counters every [`BEAT_MS`]
    /// until `cancel_all` or drop.
    pub fn with_registry(registry: Option<JobRegistry>) -> Arc<Jobs> {
        let jobs = Arc::new(Jobs {
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            registry,
            beat_stop: Arc::new(AtomicBool::new(false)),
        });
        if jobs.registry.is_some() {
            let weak = Arc::downgrade(&jobs);
            let stop = jobs.beat_stop.clone();
            std::thread::spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some(this) = weak.upgrade() else {
                    break;
                };
                this.beat_once();
                drop(this);
                std::thread::sleep(Duration::from_millis(BEAT_MS));
            });
        }
        jobs
    }

    /// One beat interval: the heartbeat of every live row of this writer
    /// plus the counters of the running jobs — two statements a beat,
    /// never one per file.
    fn beat_once(&self) {
        let Some(reg) = &self.registry else { return };
        let beats: Vec<Beat> = {
            let g = self.lock();
            g.jobs
                .iter()
                .filter(|j| j.state == State::Running)
                .map(|j| Beat {
                    job_id: j.id.clone(),
                    phase: phase_name(&j.progress).to_string(),
                    files: j.progress.files(),
                    bytes: j.progress.bytes(),
                    total_files: j.progress.total_files(),
                    total_bytes: j.progress.total_bytes(),
                })
                .collect()
        };
        let now = now_ms();
        let _ = reg.heartbeat(std::process::id(), now);
        let _ = reg.beat(&beats, now);
    }

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
            // Ids must be unique across writers: every client names its own.
            let id = format!("job-{}-{}", std::process::id(), idx + 1);
            g.jobs.push(Job {
                id: id.clone(),
                kind,
                target: target.clone(),
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
        if let Some(reg) = &self.registry {
            let _ = reg.announce(&id, kind, &target, std::process::id(), now_ms());
        }
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
                    if let Some(reg) = &self.registry {
                        let _ = reg.finish(
                            &job.id,
                            "cancelled",
                            "queued",
                            0,
                            0,
                            0,
                            0,
                            None,
                            None,
                            now_ms(),
                        );
                    }
                    self.changed.notify_all();
                    return;
                }
                if g.active.is_none() && g.queue.front() == Some(&idx) {
                    g.queue.pop_front();
                    g.active = Some(idx);
                    let job = &mut g.jobs[idx];
                    job.state = State::Running;
                    job.started = Some(Instant::now());
                    if let Some(reg) = &self.registry {
                        let _ = reg.mark_running(&job.id, now_ms());
                    }
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
        if let Some(reg) = &self.registry {
            let _ = reg.finish(
                &job.id,
                job.state.name(),
                phase_name(&job.progress),
                job.progress.files(),
                job.progress.bytes(),
                job.progress.total_files(),
                job.progress.total_bytes(),
                job.result.as_ref().map(Value::to_string).as_deref(),
                job.error
                    .as_ref()
                    .map(|e| e.to_json().to_string())
                    .as_deref(),
                now_ms(),
            );
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
        {
            let g = self.lock();
            if let Some(v) = g.jobs.iter().find(|j| j.id == id).map(Jobs::snapshot) {
                return Some(v);
            }
        }
        // A job of another process (a second MCP server): its registry row,
        // with dead writers' live-looking rows already reported `interrupted`.
        self.registry
            .as_ref()
            .and_then(|reg| reg.get(id, now_ms()).ok().flatten())
            .and_then(|row| serde_json::to_value(row).ok())
    }

    pub fn list(&self) -> Vec<Value> {
        let g = self.lock();
        let mut out: Vec<Value> = g.jobs.iter().map(Jobs::snapshot).collect();
        if let Some(reg) = &self.registry {
            if let Ok(rows) = reg.list(now_ms()) {
                for row in rows {
                    // This process's own rows are fresher in memory.
                    if g.jobs.iter().any(|j| j.id == row.job_id) {
                        continue;
                    }
                    if let Ok(v) = serde_json::to_value(row) {
                        out.push(v);
                    }
                }
            }
        }
        out
    }

    /// Whether `id` names another process's job (visible in the registry but
    /// not started by this server — e.g. for an honest `cancel_job` answer).
    pub fn foreign(&self, id: &str) -> bool {
        if self.lock().jobs.iter().any(|j| j.id == id) {
            return false;
        }
        self.registry
            .as_ref()
            .and_then(|reg| reg.get(id, now_ms()).ok().flatten())
            .is_some()
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
        // The server is going down: stop heartbeats at once so leftover rows
        // read as interrupted promptly (the drivers still write final states).
        self.beat_stop.store(true, Ordering::Relaxed);
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

impl Drop for Jobs {
    fn drop(&mut self) {
        self.beat_stop.store(true, Ordering::Relaxed);
    }
}
