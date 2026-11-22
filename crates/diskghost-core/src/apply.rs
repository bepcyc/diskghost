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

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::dupes::full_hash_with;
use crate::index::{stat_of, Index, Probe, Role, Stat};
use crate::plan::{Action, ActionKind, Expect, Plan, PlanGroup, PLAN_VERSION};

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
pub fn load_plan(path: &Path) -> io::Result<Plan> {
    let bytes = std::fs::read(path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("plan {}: {e}", path.display())))?;
    match v.get("version").and_then(serde_json::Value::as_u64) {
        Some(n) if n == u64::from(PLAN_VERSION) => {}
        Some(n) => {
            return Err(invalid(format!(
                "plan {}: version {n} is not supported (this diskghost reads version \
                 {PLAN_VERSION})",
                path.display()
            )))
        }
        None => {
            return Err(invalid(format!(
                "{}: not a diskghost plan (no version field)",
                path.display()
            )))
        }
    }
    serde_json::from_value(v).map_err(|e| invalid(format!("plan {}: {e}", path.display())))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Every line of a journal, in order (empty when there is none yet). A
/// last line cut short (killed while writing it) is ignored; any other bad
/// line is an error.
pub fn read_journal(path: &Path) -> io::Result<Vec<JournalEntry>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(e) => out.push(e),
            Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => {}
            Err(e) => {
                return Err(invalid(format!(
                    "journal {} line {}: {e}",
                    path.display(),
                    i + 1
                )))
            }
        }
    }
    Ok(out)
}

/// Each action's last journal line, and the order actions first appear.
fn last_states(entries: Vec<JournalEntry>) -> (Vec<String>, HashMap<String, JournalEntry>) {
    let mut order = Vec::new();
    let mut last: HashMap<String, JournalEntry> = HashMap::new();
    for e in entries {
        if !last.contains_key(&e.action_id) {
            order.push(e.action_id.clone());
        }
        last.insert(e.action_id.clone(), e);
    }
    (order, last)
}

/// The append-only journal file, opened on first write.
struct Journal {
    path: PathBuf,
    file: Option<File>,
}

impl Journal {
    fn new(path: PathBuf) -> Journal {
        Journal { path, file: None }
    }

    /// Append one line and make it durable before returning.
    fn append(&mut self, e: &JournalEntry) -> io::Result<()> {
        if self.file.is_none() {
            let existed = self.path.exists();
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)?;
            // A line cut short by a kill must not swallow the next one.
            let len = f.metadata()?.len();
            if len > 0 {
                let mut last = [0u8; 1];
                f.seek(SeekFrom::Start(len - 1))?;
                f.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    f.write_all(b"\n")?;
                }
            }
            if !existed {
                sync_dir(self.path.parent());
            }
            self.file = Some(f);
        }
        let f = self.file.as_mut().expect("opened above");
        let mut line = serde_json::to_vec(e).map_err(io::Error::other)?;
        line.push(b'\n');
        f.write_all(&line)?;
        f.sync_data()
    }
}

/// Best effort: make a directory's entries (a new file, a rename) durable.
fn sync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(f) = File::open(d) {
            let _ = f.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Carry out the plan at `plan_path` against the index `idx`.
pub fn apply(plan_path: &Path, idx: &Index, opts: &ApplyOptions) -> io::Result<RunReport> {
    apply_with(plan_path, idx, opts, &Probe::REAL)
}

/// [`apply`] asking `probe` whether a location is reachable.
pub(crate) fn apply_with(
    plan_path: &Path,
    idx: &Index,
    opts: &ApplyOptions,
    probe: &Probe,
) -> io::Result<RunReport> {
    let plan = load_plan(plan_path)?;
    let jpath = journal_path(plan_path);
    let (_, last) = last_states(read_journal(&jpath)?);
    let mut journal = Journal::new(jpath.clone());
    let mut outcomes = Vec::new();
    for g in &plan.groups {
        for a in &g.actions {
            let run = Run {
                plan: &plan,
                group: g,
                action: a,
                idx,
                opts,
                probe,
            };
            outcomes.push(run.go(last.get(&a.id), &mut journal)?);
        }
    }
    Ok(RunReport {
        journal: jpath,
        dry_run: opts.dry_run,
        outcomes,
    })
}

/// One action of one run.
struct Run<'a> {
    plan: &'a Plan,
    group: &'a PlanGroup,
    action: &'a Action,
    idx: &'a Index,
    opts: &'a ApplyOptions,
    probe: &'a Probe<'a>,
}

/// An action's result before it is recorded.
struct Verdict {
    status: Status,
    to: Option<PathBuf>,
    detail: Option<String>,
}

fn verdict(status: Status, detail: impl Into<String>) -> Verdict {
    Verdict {
        status,
        to: None,
        detail: Some(detail.into()),
    }
}

impl Run<'_> {
    /// Decide, act, record. `Err` only when the journal cannot be written —
    /// then nothing more may be moved.
    fn go(&self, prev: Option<&JournalEntry>, journal: &mut Journal) -> io::Result<Outcome> {
        let a = self.action;
        if let Some(p) = prev {
            match p.status {
                Status::Done => {
                    return Ok(self.outcome(Verdict {
                        status: Status::Skipped,
                        to: p.to.clone(),
                        detail: Some("done by an earlier run".into()),
                    }))
                }
                Status::Pending => {
                    if let Some(v) = self.reconcile(p) {
                        return self.record(v, journal);
                    }
                }
                _ => {}
            }
        }
        let v = match self.check() {
            Ok(target) => target,
            Err(v) => return self.record(v, journal),
        };
        if self.opts.dry_run {
            return Ok(self.outcome(v));
        }
        let to = v.to.expect("check gives the target");
        if let Err(v) = self.prepare(&to) {
            return self.record(v, journal);
        }
        journal.append(&self.entry(Status::Pending, Some(to.clone()), None))?;
        let v = match rename_noreplace(&a.path, &to) {
            Ok(()) => {
                sync_dir(to.parent());
                sync_dir(a.path.parent());
                Verdict {
                    status: Status::Done,
                    to: Some(to),
                    detail: None,
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Verdict {
                status: Status::Conflict,
                to: Some(to),
                detail: Some("destination appeared meanwhile; nothing replaced".into()),
            },
            Err(e) if is_cross_device(&e) => Verdict {
                status: Status::Error,
                to: Some(to),
                detail: Some(
                    "rename would cross devices: quarantine must be on the same volume".into(),
                ),
            },
            Err(e) => Verdict {
                status: Status::Error,
                to: Some(to),
                detail: Some(format!("rename failed: {e}")),
            },
        };
        self.record(v, journal)
    }

    /// A `pending` line without `done`: the run was killed around the
    /// rename. Moved (gone from `from`, its inode at `to`) → done; still at
    /// `from` → `None`, check and move it again.
    fn reconcile(&self, p: &JournalEntry) -> Option<Verdict> {
        let a = self.action;
        let at_from = std::fs::symlink_metadata(&a.path).is_ok();
        if at_from {
            return None;
        }
        let ino = p.ino.unwrap_or(a.expect.ino);
        let moved =
            p.to.as_ref()
                .and_then(|t| std::fs::symlink_metadata(t).ok())
                .is_some_and(|m| m.is_file() && stat_of(&m).ino == ino);
        Some(if moved {
            Verdict {
                status: if self.opts.dry_run {
                    Status::Skipped
                } else {
                    Status::Done
                },
                to: p.to.clone(),
                detail: Some("moved by an interrupted run".into()),
            }
        } else {
            verdict(
                Status::Stale,
                "an interrupted run left the file neither at its path nor in quarantine",
            )
        })
    }

    /// Every check before a move; `Ok` carries the quarantine target.
    fn check(&self) -> Result<Verdict, Verdict> {
        let (a, g) = (self.action, self.group);
        let hash_ok = g.hash.len() == 64 && g.hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !hash_ok {
            return Err(verdict(
                Status::Error,
                "group hash is not a BLAKE3 hex digest",
            ));
        }
        if a.kind != ActionKind::Quarantine {
            return Err(verdict(Status::Error, "unknown action type"));
        }
        // The location, as the index knows it now.
        let loc = self
            .idx
            .conn
            .query_row(
                "SELECT root, host, role FROM locations WHERE uuid = ?1",
                [&a.location_uuid],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| verdict(Status::Error, format!("index: {e}")))?;
        let Some((root, host, stored)) = loc else {
            return Err(verdict(
                Status::Error,
                format!("location {} is not in the index", a.location_uuid),
            ));
        };
        let root = PathBuf::from(root);
        if !matches!(host.as_str(), "local" | "localhost") {
            return Err(verdict(
                Status::Error,
                format!("location on host {host:?}: remote locations are not supported yet"),
            ));
        }
        if !(self.probe.online)(&root, &a.location_uuid) {
            return Err(verdict(
                Status::Offline,
                format!("volume {} is not reachable", a.location_uuid),
            ));
        }
        let role = self
            .plan
            .policy
            .role_for(&a.location_uuid, &host, &root, Role::parse(&stored));
        if role == Role::Master {
            return Err(verdict(
                Status::Error,
                "location is master: never quarantined",
            ));
        }
        // The path: inside its location, plainly, and not in quarantine.
        let plain = a.path.is_absolute()
            && a.path
                .components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
        let rel = a.path.strip_prefix(&root).ok().filter(|_| plain);
        let Some(rel) = rel.filter(|r| r.file_name().is_some()) else {
            return Err(verdict(
                Status::Error,
                format!("path is not a plain path inside {}", root.display()),
            ));
        };
        if rel.components().any(|c| c.as_os_str() == QUARANTINE_DIR) {
            return Err(verdict(Status::Error, "path is already in quarantine"));
        }
        let (Ok(croot), Some(Ok(cparent))) = (
            std::fs::canonicalize(&root),
            a.path.parent().map(std::fs::canonicalize),
        ) else {
            return Err(verdict(Status::Stale, "its folder is gone"));
        };
        if !cparent.starts_with(&croot) {
            return Err(verdict(
                Status::Error,
                "its folder resolves outside the location (symlink)",
            ));
        }
        // Still the file of the plan?
        let st = self.stat_matches(&a.path, &a.expect)?;
        // Still the content of the group?
        let Some(full) = full_hash_with(&a.path, st.size, true) else {
            return Err(verdict(Status::Error, "cannot read the file"));
        };
        let hex = blake3::Hash::from(full).to_hex().to_string();
        if hex != g.hash || hex != a.expect.full {
            return Err(verdict(
                Status::HashMismatch,
                format!("content hashes to {hex}, not the group's {}", g.hash),
            ));
        }
        self.stat_matches(&a.path, &a.expect)
            .map_err(|_| verdict(Status::Stale, "changed while being read"))?;
        if self.opts.paranoid {
            self.paranoid(&st)?;
        }
        // Where it goes.
        let dir = root.join(QUARANTINE_DIR).join(&g.hash[..16]);
        let base = a.path.file_name().expect("checked above");
        let mut name = base.to_os_string();
        name.push(format!(".{}", st.ino));
        let mut to = dir.join(&name);
        let mut n = 0;
        while std::fs::symlink_metadata(&to).is_ok() {
            n += 1;
            if n > 999 {
                return Err(verdict(Status::Conflict, "no free name in quarantine"));
            }
            let mut alt = name.clone();
            alt.push(format!(".{n}"));
            to = dir.join(alt);
        }
        Ok(Verdict {
            status: Status::WouldQuarantine,
            to: Some(to),
            detail: None,
        })
    }

    /// `path` is a regular file matching `expect` (size, mtime, device,
    /// inode); otherwise the stale verdict saying what differs.
    fn stat_matches(&self, path: &Path, expect: &Expect) -> Result<Stat, Verdict> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(verdict(Status::Stale, "gone"))
            }
            Err(e) => return Err(verdict(Status::Error, format!("cannot stat: {e}"))),
        };
        if !meta.is_file() {
            return Err(verdict(Status::Stale, "no longer a regular file"));
        }
        let st = stat_of(&meta);
        let mut diff = Vec::new();
        if st.size != expect.size {
            diff.push(format!("size {} → {}", expect.size, st.size));
        }
        if st.mtime_ns != expect.mtime_ns {
            diff.push("mtime".to_string());
        }
        if st.dev != expect.dev {
            diff.push("device".to_string());
        }
        if st.ino != expect.ino {
            diff.push("inode".to_string());
        }
        if diff.is_empty() {
            Ok(st)
        } else {
            Err(verdict(
                Status::Stale,
                format!("changed since the plan: {}", diff.join(", ")),
            ))
        }
    }

    /// Byte-compare the target with a kept copy that is reachable, local,
    /// unchanged since the plan and not the target's own inode.
    fn paranoid(&self, st: &Stat) -> Result<(), Verdict> {
        let a = self.action;
        for k in &self.group.kept {
            let Some(x) = k.expect.as_ref().filter(|_| k.available) else {
                continue;
            };
            let local = self
                .plan
                .locations
                .iter()
                .find(|l| l.uuid == k.location_uuid)
                .is_some_and(|l| matches!(l.host.as_str(), "local" | "localhost"));
            if !local || k.path == a.path || (x.dev, x.ino) == (st.dev, st.ino) {
                continue;
            }
            if self.stat_matches(&k.path, x).is_err() {
                continue;
            }
            match same_bytes(&a.path, &k.path) {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    return Err(verdict(
                        Status::ParanoidMismatch,
                        format!("differs byte-wise from kept copy {}", k.path.display()),
                    ))
                }
                Err(_) => continue,
            }
        }
        Err(verdict(
            Status::ParanoidNoReference,
            "no reachable, unchanged kept copy to compare with",
        ))
    }

    /// Make the quarantine folders (real folders, on the file's device).
    fn prepare(&self, to: &Path) -> Result<(), Verdict> {
        let dir = to.parent().expect("target has a folder");
        let top = dir.parent().expect("quarantine has a root");
        for d in [top, dir] {
            match std::fs::create_dir(d) {
                Ok(()) => sync_dir(d.parent()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(verdict(
                        Status::Error,
                        format!("cannot create {}: {e}", d.display()),
                    ))
                }
            }
            match std::fs::symlink_metadata(d) {
                Ok(m) if m.is_dir() => {}
                _ => {
                    return Err(verdict(
                        Status::Error,
                        format!("{} is not a plain folder", d.display()),
                    ))
                }
            }
        }
        let qdev = std::fs::symlink_metadata(dir).map(|m| stat_of(&m).dev);
        if qdev.ok() != Some(self.action.expect.dev) {
            return Err(verdict(
                Status::Error,
                "quarantine folder is on another device than the file",
            ));
        }
        Ok(())
    }

    fn entry(&self, status: Status, to: Option<PathBuf>, detail: Option<String>) -> JournalEntry {
        JournalEntry {
            action_id: self.action.id.clone(),
            from: self.action.path.clone(),
            to,
            group: self.group.hash.clone(),
            status,
            ts: now(),
            ino: Some(self.action.expect.ino),
            detail,
        }
    }

    fn outcome(&self, v: Verdict) -> Outcome {
        Outcome {
            action_id: self.action.id.clone(),
            from: self.action.path.clone(),
            to: v.to,
            status: v.status,
            detail: v.detail,
        }
    }

    /// Journal the verdict (not in a dry run, not a mere skip) and report it.
    fn record(&self, v: Verdict, journal: &mut Journal) -> io::Result<Outcome> {
        if !self.opts.dry_run && v.status != Status::Skipped {
            journal.append(&self.entry(v.status, v.to.clone(), v.detail.clone()))?;
        }
        Ok(self.outcome(v))
    }
}

/// Whether two files hold the same bytes, read in step a chunk at a time.
fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    const CHUNK: usize = 1 << 20;
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// Fill `buf` unless the file ends first; how many bytes were read.
fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// `rename(from, to)` that fails with `AlreadyExists` instead of replacing
/// an existing `to` — `renameat2(RENAME_NOREPLACE)`, atomically, where the
/// kernel and filesystem support it; elsewhere a check right before a plain
/// rename (a window only another writer racing into the very same
/// quarantine name could hit).
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        const RENAME_NOREPLACE: libc::c_uint = 1;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let t = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: both pointers are valid NUL-terminated strings for the
        // whole call; renameat2 only reads them.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        // No renameat2 (old kernel) or no RENAME_NOREPLACE (some FUSE/NFS).
        if !matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Err(e);
        }
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists", to.display()),
        ));
    }
    std::fs::rename(from, to)
}

#[cfg(unix)]
fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(not(unix))]
fn is_cross_device(_e: &io::Error) -> bool {
    false
}

/// Move every quarantined file of `journal` back where it was, newest
/// first. A file whose old name is taken again, or whose quarantined copy is
/// not the moved inode, is left where it is and reported.
pub fn undo(journal: &Path, dry_run: bool) -> io::Result<RunReport> {
    let (order, last) = last_states(read_journal(journal)?);
    let mut out = Journal::new(journal.to_path_buf());
    let mut outcomes = Vec::new();
    for id in order.iter().rev() {
        let e = &last[id];
        let report = |status: Status, detail: Option<String>| Outcome {
            action_id: e.action_id.clone(),
            from: e.from.clone(),
            to: e.to.clone(),
            status,
            detail,
        };
        match e.status {
            Status::Undone => {
                outcomes.push(report(Status::Skipped, Some("already undone".into())));
                continue;
            }
            Status::Done | Status::Pending => {}
            _ => continue, // never moved
        }
        let Some(to) = e.to.as_ref() else {
            outcomes.push(report(
                Status::Error,
                Some("journal line has no `to`".into()),
            ));
            continue;
        };
        let meta = match std::fs::symlink_metadata(to) {
            Ok(m) => m,
            Err(_) if e.status == Status::Pending => {
                outcomes.push(report(Status::Skipped, Some("never moved".into())));
                continue;
            }
            Err(err) => {
                outcomes.push(report(
                    Status::Error,
                    Some(format!("quarantined file {}: {err}", to.display())),
                ));
                continue;
            }
        };
        if !meta.is_file() || e.ino.is_some_and(|i| i != stat_of(&meta).ino) {
            outcomes.push(report(
                Status::Conflict,
                Some(format!("{} is not the moved file", to.display())),
            ));
            continue;
        }
        if std::fs::symlink_metadata(&e.from).is_ok() {
            outcomes.push(report(
                Status::Conflict,
                Some("a file has taken its old name; nothing replaced".into()),
            ));
            continue;
        }
        if dry_run {
            outcomes.push(report(Status::WouldRestore, None));
            continue;
        }
        if let Some(parent) = e.from.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                outcomes.push(report(
                    Status::Error,
                    Some(format!("cannot recreate {}: {err}", parent.display())),
                ));
                continue;
            }
        }
        match rename_noreplace(to, &e.from) {
            Ok(()) => {
                sync_dir(e.from.parent());
                sync_dir(to.parent());
                out.append(&JournalEntry {
                    status: Status::Undone,
                    ts: now(),
                    detail: None,
                    ..e.clone()
                })?;
                outcomes.push(report(Status::Undone, None));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => outcomes.push(report(
                Status::Conflict,
                Some("a file has taken its old name; nothing replaced".into()),
            )),
            Err(err) => outcomes.push(report(Status::Error, Some(format!("rename failed: {err}")))),
        }
    }
    Ok(RunReport {
        journal: journal.to_path_buf(),
        dry_run,
        outcomes,
    })
}
