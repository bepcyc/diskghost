//! `apply` and `undo`: carry out a plan's quarantine actions, and take them
//! back.
//!
//! # Safety
//!
//! The only change this module makes to a user's file is `rename` — into
//! `<location root>/.diskghost-quarantine/` on the same volume, and back.
//! It never unlinks, truncates or writes a user file, and never replaces
//! one: every rename refuses an existing destination (`RENAME_NOREPLACE`).
//! A rename that would cross devices fails; there is no copy fallback.
//!
//! # Journal
//!
//! `<plan>.journal.jsonl`, one JSON object per line, each fsync'd:
//! `{action_id, from, to, group, status, ts, ino, detail}`. Before a rename a
//! `pending` line, after it `done`; a later `undo` appends `undone`. An
//! action's state is its *last* line: `done` is skipped by a re-run (the
//! resume mechanism); `pending` without `done` (killed mid-rename) is
//! reconciled from the filesystem.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::index::{Index, Probe};
use crate::plan::Plan;

/// Name of the quarantine directory at each location's root.
pub const QUARANTINE_DIR: &str = ".diskghost-quarantine";

/// What `apply` should do besides the checks it always makes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApplyOptions {
    /// Check and report; touch nothing (no journal, no directory).
    pub dry_run: bool,
    /// Before moving, compare the file byte by byte with a kept copy.
    pub paranoid: bool,
}

/// What happened to an action (journal status or run outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// About to rename (journal only).
    Pending,
    /// Moved into quarantine.
    Done,
    /// Restored by `undo`.
    Undone,
    /// Already done by an earlier run (or already undone): nothing to do.
    Skipped,
    /// `--dry-run`: would be moved to `to`.
    WouldQuarantine,
    /// `undo --dry-run`: would be moved back to `from`.
    WouldRestore,
    /// Gone, or size/mtime/device/inode differ from the plan: untouched.
    Stale,
    /// Its content no longer hashes to the group's hash: untouched.
    HashMismatch,
    /// `--paranoid`: differs byte-wise from the kept copy: untouched.
    ParanoidMismatch,
    /// `--paranoid`: no kept copy is reachable and unchanged to compare
    /// with: untouched.
    ParanoidNoReference,
    /// Its location is not reachable now: untouched.
    Offline,
    /// The destination exists (never replaced): untouched.
    Conflict,
    /// Refused or failed (master location, path outside its location,
    /// permissions, cross-device…): untouched.
    Error,
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub action_id: String,
    pub from: PathBuf,
    #[serde(default)]
    pub to: Option<PathBuf>,
    /// The group's hash.
    pub group: String,
    pub status: Status,
    /// Unix seconds.
    pub ts: u64,
    /// Inode of the moved file (`undo` checks it).
    #[serde(default)]
    pub ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One action's result in this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub action_id: String,
    pub from: PathBuf,
    pub to: Option<PathBuf>,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What `apply` or `undo` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    pub journal: PathBuf,
    pub dry_run: bool,
    pub outcomes: Vec<Outcome>,
}

impl RunReport {
    /// How many outcomes have `status`.
    pub fn count(&self, status: Status) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }

    /// Something was refused or failed (not merely skipped as changed).
    pub fn has_errors(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o.status, Status::Error | Status::Conflict))
    }
}

/// The journal of a plan: `<plan>.journal.jsonl` beside it.
pub fn journal_path(plan: &Path) -> PathBuf {
    let mut s = plan.as_os_str().to_os_string();
    s.push(".journal.jsonl");
    PathBuf::from(s)
}

/// Read a plan file, refusing a version this build does not know.
pub fn load_plan(_path: &Path) -> io::Result<Plan> {
    Err(io::Error::other("load_plan not implemented"))
}

/// Every line of a journal, in order (empty when there is none yet).
pub fn read_journal(_path: &Path) -> io::Result<Vec<JournalEntry>> {
    Err(io::Error::other("read_journal not implemented"))
}

/// Carry out the plan at `plan_path` against the index `idx`.
pub fn apply(plan_path: &Path, idx: &Index, opts: &ApplyOptions) -> io::Result<RunReport> {
    apply_with(plan_path, idx, opts, &Probe::REAL)
}

/// [`apply`] asking `probe` whether a location is reachable.
pub(crate) fn apply_with(
    _plan_path: &Path,
    _idx: &Index,
    _opts: &ApplyOptions,
    _probe: &Probe,
) -> io::Result<RunReport> {
    Err(io::Error::other("apply not implemented"))
}

/// Move every quarantined file of `journal` back where it was.
pub fn undo(_journal: &Path, _dry_run: bool) -> io::Result<RunReport> {
    Err(io::Error::other("undo not implemented"))
}
