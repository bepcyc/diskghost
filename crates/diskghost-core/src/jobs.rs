//! The shared job registry: one small SQLite database that every diskghost
//! process running index jobs records them in, so any other process — a
//! second `diskghost mcp` server, the GUI — can see the jobs of them all.
//!
//! It is deliberately **not** a table in `index.db`: a local scan holds the
//! index's single write lock inside one transaction for the whole walk
//! (see [`crate::index`]'s `scan_with`), so a second connection could not
//! record a queued job, a heartbeat or a counter update during exactly the
//! hours progress matters — every such write would end in `SQLITE_BUSY`
//! after its timeout, and a healthy running scan would look dead. The
//! registry is therefore a sibling file `index-jobs.db` (WAL, one table)
//! whose lock never competes with the scan's.
//!
//! A row is written by the process that runs the job (the "writer"): on
//! submission, on every state transition, and — on a heartbeat thread —
//! on a fixed beat interval with the live counters and a fresh `heartbeat`
//! timestamp. A row that stays `queued`/`running` while its `heartbeat`
//! stops is a job whose writer died: after [`STALE_MS`] it is reported as
//! `interrupted` (never `running` forever), and a fresh writer marks such
//! leftovers permanently via [`JobRegistry::interrupt_stale`]. Liveness is
//! judged by heartbeat age alone — no process-table check — so it works
//! identically on every platform.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::index::db_err;

/// How long a `queued`/`running` row's heartbeat may go unrefreshed before
/// readers report it `interrupted`. Beats arrive every few seconds; 15 s
/// tolerates a busy machine and a short scheduler stall.
pub const STALE_MS: i64 = 15_000;

/// How long a finished row is kept before [`JobRegistry::prune_finished`]
/// deletes it — the registry stays a look at the recent past, not an archive.
pub const RETAIN_MS: i64 = 24 * 60 * 60 * 1000;

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The registry file belonging to index `index_db` — a sibling named after
/// it (`index.db` → `index-jobs.db`), so every index has its own jobs.
pub fn jobs_db_path(index_db: &Path) -> PathBuf {
    let stem = index_db
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("index");
    index_db.with_file_name(format!("{stem}-jobs.db"))
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS jobs (
    job_id      TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    target      TEXT NOT NULL,
    state       TEXT NOT NULL
                CHECK (state IN ('queued','running','done','failed','cancelled','interrupted')),
    phase       TEXT NOT NULL DEFAULT 'walk',
    files       INTEGER NOT NULL DEFAULT 0,
    bytes       INTEGER NOT NULL DEFAULT 0,
    total_files INTEGER NOT NULL DEFAULT 0,
    total_bytes INTEGER NOT NULL DEFAULT 0,
    result      TEXT,                    -- the job's JSON result, at finish
    error       TEXT,                    -- the job's JSON error, at finish
    queued_at   INTEGER NOT NULL,        -- unix ms the job was submitted
    started_at  INTEGER,                 -- unix ms it entered 'running'
    ended_at    INTEGER,                 -- unix ms it finished / was interrupted
    writer_pid  INTEGER NOT NULL,        -- process that owns (owned) it
    heartbeat   INTEGER NOT NULL         -- unix ms the writer last touched it
);
PRAGMA user_version = 1;
";

/// A job as any reader sees it — the same fields the MCP `job_status` /
/// `list_jobs` tools and the GUI's sidebar show. `queued_ms` and
/// `elapsed_ms` are *durations* computed from the stored timestamps at read
/// time; `state` already folds in the staleness rule.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JobRow {
    pub job_id: String,
    pub kind: String,
    pub target: String,
    /// `queued`, `running`, `done`, `failed`, `cancelled` — or `interrupted`
    /// when the row looked live but its writer's heartbeat went stale.
    pub state: String,
    pub phase: String,
    pub files: u64,
    pub bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
    /// Milliseconds the job spent (or is spending) queued.
    pub queued_ms: u64,
    /// Milliseconds the job ran (or has been running).
    pub elapsed_ms: u64,
    /// Process id of the writer that owns this job.
    pub writer_pid: u32,
    /// Unix ms of the writer's last update.
    pub heartbeat_ms: i64,
    /// The job's result (finished jobs only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The job's error (finished jobs only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

/// One live job's counters, handed to [`JobRegistry::beat`] by the writer's
/// heartbeat thread — read from `Progress` once per interval, never per file.
pub struct Beat {
    pub job_id: String,
    pub phase: String,
    pub files: u64,
    pub bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
}

/// The registry: an open `index-jobs.db`. `Send + Sync` — the connection
/// lives behind a mutex, so a writer's driver and heartbeat threads can
/// share it.
pub struct JobRegistry {
    conn: Mutex<Connection>,
}

impl JobRegistry {
    /// The writer side: open the registry of `index_db`, creating it (and
    /// its directory) when missing.
    pub fn for_index(index_db: &Path) -> io::Result<JobRegistry> {
        JobRegistry::open(&jobs_db_path(index_db))
    }

    /// The reader side: open the registry of `index_db` read-only, and only
    /// when it already exists — a reader never creates the file, never runs
    /// the schema batch and never changes the journal mode, so opening it
    /// cannot write a byte to the registry.
    pub fn for_index_reader(index_db: &Path) -> io::Result<Option<JobRegistry>> {
        let path = jobs_db_path(index_db);
        if !path.exists() {
            return Ok(None);
        }
        let conn =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        Ok(Some(JobRegistry {
            conn: Mutex::new(conn),
        }))
    }

    /// Open the registry at `path`, creating it if needed.
    pub fn open(path: &Path) -> io::Result<JobRegistry> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_err)?;
        // Like the index: readers never wait for a writer, and vice versa.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        Ok(JobRegistry {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a just-submitted job (`queued`); `job_id` is unique per writer.
    pub fn announce(
        &self,
        job_id: &str,
        kind: &str,
        target: &str,
        writer_pid: u32,
        now: i64,
    ) -> io::Result<()> {
        // REPLACE, not plain INSERT: a process id can be reused, so a stale
        // row of the same id from a dead writer must not break the insert.
        self.lock()
            .execute(
                "INSERT OR REPLACE INTO jobs \
                 (job_id, kind, target, state, phase, writer_pid, queued_at, heartbeat) \
                 VALUES (?1, ?2, ?3, 'queued', 'queued', ?4, ?5, ?5)",
                params![job_id, kind, target, writer_pid, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// The job left the queue and runs now.
    pub fn mark_running(&self, job_id: &str, now: i64) -> io::Result<()> {
        self.lock()
            .execute(
                "UPDATE jobs SET state = 'running', started_at = ?2, heartbeat = ?2 \
                 WHERE job_id = ?1",
                params![job_id, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// The job reached an end state, with its final counters and its result
    /// or error (each already JSON).
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        job_id: &str,
        state: &str,
        phase: &str,
        files: u64,
        bytes: u64,
        total_files: u64,
        total_bytes: u64,
        result: Option<&str>,
        error: Option<&str>,
        now: i64,
    ) -> io::Result<()> {
        self.lock()
            .execute(
                "UPDATE jobs SET state = ?2, phase = ?3, files = ?4, bytes = ?5, \
                 total_files = ?6, total_bytes = ?7, result = ?8, error = ?9, \
                 ended_at = ?10, heartbeat = ?10 WHERE job_id = ?1",
                params![
                    job_id,
                    state,
                    phase,
                    files as i64,
                    bytes as i64,
                    total_files as i64,
                    total_bytes as i64,
                    result,
                    error,
                    now
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// Refresh the heartbeat of every live row of writer `pid` — one
    /// statement for the whole queue. Called on the writer's beat interval.
    pub fn heartbeat(&self, writer_pid: u32, now: i64) -> io::Result<()> {
        self.lock()
            .execute(
                "UPDATE jobs SET heartbeat = ?2 WHERE writer_pid = ?1 \
                 AND state IN ('queued', 'running')",
                params![writer_pid, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// One interval's counters for the running jobs of this writer.
    pub fn beat(&self, beats: &[Beat], now: i64) -> io::Result<()> {
        let conn = self.lock();
        let mut st = conn
            .prepare_cached(
                "UPDATE jobs SET phase = ?2, files = ?3, bytes = ?4, total_files = ?5, \
                 total_bytes = ?6, heartbeat = ?7 WHERE job_id = ?1 AND state = 'running'",
            )
            .map_err(db_err)?;
        for b in beats {
            st.execute(params![
                b.job_id,
                b.phase,
                b.files as i64,
                b.bytes as i64,
                b.total_files as i64,
                b.total_bytes as i64,
                now
            ])
            .map_err(db_err)?;
        }
        Ok(())
    }

    /// A fresh writer start: rows that still look live but whose heartbeat
    /// is already stale belong to dead processes — mark them `interrupted`
    /// so no reader has to. Live other writers have fresh heartbeats and are
    /// untouched. `ended_at` takes the last sign of life, not now — nothing
    /// later is actually known.
    pub fn interrupt_stale(&self, now: i64) -> io::Result<u64> {
        let n = self
            .lock()
            .execute(
                "UPDATE jobs SET state = 'interrupted', ended_at = heartbeat \
                 WHERE state IN ('queued', 'running') AND heartbeat < ?1",
                [now - STALE_MS],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    /// Forget finished rows older than `retain_ms`; live rows are never pruned.
    pub fn prune_finished(&self, now: i64, retain_ms: i64) -> io::Result<u64> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM jobs WHERE state IN ('done','failed','cancelled','interrupted') \
                 AND COALESCE(ended_at, queued_at) < ?1",
                [now - retain_ms],
            )
            .map_err(db_err)?;
        Ok(n as u64)
    }

    /// Every job, oldest submitted first, with `state` already folded by the
    /// staleness rule (`now` = the reader's clock, injectable for tests).
    pub fn list(&self, now: i64) -> io::Result<Vec<JobRow>> {
        let conn = self.lock();
        let mut st = conn
            .prepare(
                "SELECT job_id, kind, target, state, phase, files, bytes, total_files, \
                 total_bytes, result, error, queued_at, started_at, ended_at, \
                 writer_pid, heartbeat FROM jobs ORDER BY queued_at, job_id",
            )
            .map_err(db_err)?;
        let rows = st.query_map([], |r| Ok(row_of(r, now))).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)
    }

    /// [`JobRegistry::list`] for one job id.
    pub fn get(&self, job_id: &str, now: i64) -> io::Result<Option<JobRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT job_id, kind, target, state, phase, files, bytes, total_files, \
             total_bytes, result, error, queued_at, started_at, ended_at, \
             writer_pid, heartbeat FROM jobs WHERE job_id = ?1",
            [job_id],
            |r| Ok(row_of(r, now)),
        )
        .optional()
        .map_err(db_err)
    }
}

/// Build a [`JobRow`] from a selected database row, folding the staleness
/// rule into `state` and deriving the duration fields the responses use.
fn row_of(r: &rusqlite::Row, now: i64) -> JobRow {
    let get = |i: usize| r.get::<_, i64>(i).unwrap_or(0);
    let gets = |i: usize| r.get::<_, String>(i).unwrap_or_default();
    let (state_col, queued_at, started_at, ended_at, heartbeat) =
        (gets(3), get(11), get(12), get(13), get(15));
    // The one staleness rule, here and in `interrupt_stale`: a row that
    // claims life but has not been touched for STALE_MS has a dead writer.
    let live = matches!(state_col.as_str(), "queued" | "running") && now - heartbeat <= STALE_MS;
    let state = if matches!(state_col.as_str(), "queued" | "running") && !live {
        "interrupted".to_string()
    } else {
        state_col
    };
    let end_or_now = |ms: i64| if ms > 0 { ms } else { now };
    JobRow {
        job_id: gets(0),
        kind: gets(1),
        target: gets(2),
        phase: if state == "queued" {
            "queued".into()
        } else {
            gets(4)
        },
        files: get(5) as u64,
        bytes: get(6) as u64,
        total_files: get(7) as u64,
        total_bytes: get(8) as u64,
        result: r
            .get::<_, Option<String>>(9)
            .unwrap_or(None)
            .and_then(|s| serde_json::from_str(&s).ok()),
        error: r
            .get::<_, Option<String>>(10)
            .unwrap_or(None)
            .and_then(|s| serde_json::from_str(&s).ok()),
        queued_ms: (end_or_now(if started_at > 0 { started_at } else { ended_at }) - queued_at)
            .max(0) as u64,
        elapsed_ms: if started_at > 0 {
            (end_or_now(ended_at) - started_at).max(0) as u64
        } else {
            0
        },
        writer_pid: get(14) as u32,
        heartbeat_ms: heartbeat,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch registry file owned by this test alone.
    fn tmpdir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diskghost-jobs-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn registry(label: &str) -> (PathBuf, JobRegistry) {
        let d = tmpdir(label);
        let db = d.join("index.db");
        let reg = JobRegistry::for_index(&db).unwrap();
        assert_eq!(jobs_db_path(&db), d.join("index-jobs.db"));
        (d, reg)
    }

    #[test]
    fn an_announced_job_is_seen_by_another_processes_registry() {
        // The cross-process read path: writer "pid 1" announces, a second
        // JobRegistry on the same file reads it back.
        let (d, writer) = registry("two-proc");
        writer
            .announce("job-1-1", "scan", "/data", 1, 1_000)
            .unwrap();
        let reader = JobRegistry::open(&d.join("index-jobs.db")).unwrap();
        let rows = reader.list(1_001).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(
            (
                r.job_id.as_str(),
                r.kind.as_str(),
                r.target.as_str(),
                r.state.as_str()
            ),
            ("job-1-1", "scan", "/data", "queued")
        );
        assert_eq!(r.phase, "queued");
        assert_eq!(r.writer_pid, 1);
        assert_eq!((r.queued_ms, r.elapsed_ms), (1, 0));
        assert!(reader.get("job-1-1", 1_001).unwrap().is_some());
        assert!(reader.get("job-nope", 1_001).unwrap().is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn transitions_record_state_counters_and_outcome() {
        let (_d, reg) = registry("transitions");
        reg.announce("job-7-1", "scan", "/x", 7, 100).unwrap();
        reg.mark_running("job-7-1", 200).unwrap();
        reg.beat(
            &[Beat {
                job_id: "job-7-1".into(),
                phase: "walk".into(),
                files: 42,
                bytes: 4096,
                total_files: 0,
                total_bytes: 0,
            }],
            250,
        )
        .unwrap();
        reg.finish(
            "job-7-1",
            "done",
            "walk",
            50,
            5000,
            0,
            0,
            Some("{\"summaries\":[]}"),
            None,
            900,
        )
        .unwrap();
        let j = reg.get("job-7-1", 1_000).unwrap().unwrap();
        assert_eq!(j.state, "done");
        assert_eq!((j.files, j.bytes), (50, 5000));
        assert_eq!((j.queued_ms, j.elapsed_ms), (100, 700));
        assert_eq!(j.result, Some(serde_json::json!({"summaries": []})));
        assert!(j.error.is_none());
        // A failed job carries its error JSON instead.
        reg.announce("job-7-2", "dupes_report", "all", 7, 100)
            .unwrap();
        reg.mark_running("job-7-2", 110).unwrap();
        reg.finish(
            "job-7-2",
            "failed",
            "walk",
            0,
            0,
            0,
            0,
            None,
            Some("{\"code\":\"index_busy\"}"),
            300,
        )
        .unwrap();
        let j = reg.get("job-7-2", 400).unwrap().unwrap();
        assert_eq!(j.state, "failed");
        assert_eq!(j.error, Some(serde_json::json!({"code": "index_busy"})));
        std::fs::remove_dir_all(_d).ok();
    }

    #[test]
    fn a_live_row_with_a_stale_heartbeat_reports_interrupted() {
        // The dead-writer rule: still 'running' while the heartbeat is
        // fresh, 'interrupted' the read after it ages past STALE_MS — and
        // finished rows are never re-labelled.
        let (_d, reg) = registry("stale");
        reg.announce("job-9-1", "scan", "/gone", 9, 1_000).unwrap();
        reg.mark_running("job-9-1", 1_100).unwrap();
        let t = 1_100 + STALE_MS;
        assert_eq!(reg.get("job-9-1", t).unwrap().unwrap().state, "running");
        assert_eq!(
            reg.get("job-9-1", t + 1).unwrap().unwrap().state,
            "interrupted"
        );
        // Interrupted keeps elapsed time at the last heartbeat, not now.
        reg.announce("job-9-2", "scan", "/ok", 9, 1_000).unwrap();
        reg.finish("job-9-2", "done", "walk", 1, 1, 0, 0, None, None, 2_000)
            .unwrap();
        assert_eq!(reg.get("job-9-2", t + 100).unwrap().unwrap().state, "done");
        std::fs::remove_dir_all(_d).ok();
    }

    #[test]
    fn a_fresh_writer_marks_only_dead_writers_rows_interrupted() {
        let (_d, reg) = registry("interrupt");
        // Dead writer's leftovers: queued and running, both silent too long.
        reg.announce("job-1-1", "scan", "/a", 1, 0).unwrap();
        reg.announce("job-1-2", "scan", "/b", 1, 0).unwrap();
        reg.mark_running("job-1-2", 0).unwrap();
        // A live other writer's jobs — heartbeats are fresh.
        reg.announce("job-2-1", "scan", "/c", 2, STALE_MS + 5_000)
            .unwrap();
        reg.announce("job-2-2", "scan", "/d", 2, STALE_MS + 5_000)
            .unwrap();
        reg.mark_running("job-2-2", STALE_MS + 5_001).unwrap();
        let now = STALE_MS + 10_000;
        assert_eq!(reg.interrupt_stale(now).unwrap(), 2);
        assert_eq!(
            reg.get("job-1-1", now).unwrap().unwrap().state,
            "interrupted"
        );
        assert_eq!(
            reg.get("job-1-2", now).unwrap().unwrap().state,
            "interrupted"
        );
        assert_eq!(reg.get("job-2-1", now).unwrap().unwrap().state, "queued");
        assert_eq!(reg.get("job-2-2", now).unwrap().unwrap().state, "running");
        // ended_at records the last sign of life (0), not the marking time.
        assert_eq!(
            reg.get("job-1-1", now + 99_999).unwrap().unwrap().state,
            "interrupted"
        );
        std::fs::remove_dir_all(_d).ok();
    }

    #[test]
    fn prune_drops_only_old_finished_rows() {
        let (_d, reg) = registry("prune");
        let day = 86_400_000;
        reg.announce("old-done", "scan", "/a", 1, 0).unwrap();
        reg.finish("old-done", "done", "walk", 0, 0, 0, 0, None, None, 1_000)
            .unwrap();
        reg.announce("new-done", "scan", "/b", 1, day - 500)
            .unwrap();
        reg.finish("new-done", "done", "walk", 0, 0, 0, 0, None, None, day)
            .unwrap();
        reg.announce("live", "scan", "/c", 1, 0).unwrap(); // stale but live state
        assert_eq!(reg.prune_finished(day * 2, RETAIN_MS).unwrap(), 1);
        assert!(reg.get("old-done", day * 2).unwrap().is_none());
        assert!(reg.get("new-done", day * 2).unwrap().is_some());
        assert!(reg.get("live", day * 2).unwrap().is_some());
        std::fs::remove_dir_all(_d).ok();
    }

    #[test]
    fn a_missing_registry_reads_as_empty_not_created() {
        let d = tmpdir("absent");
        assert!(JobRegistry::for_index_reader(&d.join("index.db"))
            .unwrap()
            .is_none());
        assert!(
            !d.join("index-jobs.db").exists(),
            "a reader creates nothing"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_read_only_open_reads_an_existing_registry() {
        // The GUI's path: open_with_flags READ_ONLY — no file creation, no
        // schema batch — must still list a live writer's rows.
        let (d, writer) = registry("ro-read");
        writer
            .announce("job-1-1", "scan", "/data", 1, 1_000)
            .unwrap();
        let reader = JobRegistry::for_index_reader(&d.join("index.db"))
            .unwrap()
            .expect("registry exists");
        let rows = reader.list(1_001).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].job_id, "job-1-1");
        assert_eq!(rows[0].state, "queued");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_registry_file_lives_beside_its_index() {
        assert_eq!(
            jobs_db_path(Path::new("/data/diskghost/index.db")),
            Path::new("/data/diskghost/index-jobs.db")
        );
        assert_eq!(
            jobs_db_path(Path::new("/x/scratch-index.db")),
            Path::new("/x/scratch-index-jobs.db")
        );
        assert_eq!(jobs_db_path(Path::new("/x/db")), Path::new("/x/db-jobs.db"));
    }
}
